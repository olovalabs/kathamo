//! Mirrors `packages/coding-agent/src/core/tools/grep.ts` — the built-in `grep`
//! `AgentTool`. Searches file contents for a regex/literal pattern and returns
//! matching lines with `file:line: text` (and optional `-line-` context lines).
//! Read-only — goes through the `FileSystem` trait so it runs against both
//! `OsExecutionEnv` and `InMemoryExecutionEnv`.
//!
//! Searches in-process through the filesystem backend. Native directory
//! searches honor ignore rules and prefetch file contents in bounded batches,
//! while matches are collected in deterministic breadth-first order.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, StreamExt};
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::env::FileKind;
use crate::path_utils::resolve_read_tool_path;
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{
    format_size, truncate_head, truncate_line_default, TruncationOptions, TruncationResult,
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH,
};

/// Default max matches. Mirrors TS `grep.ts::DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 100;
const PREFETCH_FILES: usize = 8;

/// Input for the grep tool. Mirrors TS `GrepToolInput`. `camelCase` field names
/// match the TS wire schema (so `ignoreCase`/`ignore_case` deserialization lines
/// up with what the model emits).
#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepInput {
    /// Search pattern (regex or literal string).
    pub pattern: String,
    /// Directory or file to search (default: current directory).
    #[serde(default)]
    pub path: Option<String>,
    /// Filter files by glob pattern, e.g. `*.ts` or `**/*.spec.ts`.
    #[serde(default)]
    pub glob: Option<String>,
    /// Case-insensitive search (default false).
    #[serde(default)]
    pub ignore_case: Option<bool>,
    /// Treat pattern as a literal string instead of regex (default false).
    #[serde(default)]
    pub literal: Option<bool>,
    /// Number of lines to show before and after each match (default 0).
    #[serde(default)]
    pub context: Option<u32>,
    /// Maximum number of matches to return (default 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Structured details for the grep tool. Mirrors TS `GrepToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrepToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_limit_reached: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_truncated: Option<bool>,
}

/// Options for `create_grep_tool`. The TS tool carries an optional `operations`
/// bag for remote backends; the Rust port always uses the `FileSystem` trait, so
/// this is an empty forward-compat seam (parity with `ReadToolOptions`).
#[derive(Debug, Clone, Default)]
pub struct GrepToolOptions {}

/// The built-in `grep` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view).
pub struct GrepTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
}

impl GrepTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(GrepInput);
        Tool {
            name: "grep".to_string(),
            description: format!(
                "Search file contents for a pattern. Returns matching lines with file paths and \
                 line numbers. Native directory searches respect ignore rules. Output is truncated to {DEFAULT_LIMIT} \
                 matches or {kb}KB (whichever is hit first). Long lines are truncated to \
                 {GREP_MAX_LINE_LENGTH} chars.",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(params).unwrap_or_default(),
            ),
            constrained_sampling: None,
        }
    }
}

/// Build the `grep` tool from a context. Mirrors TS `createGrepTool`.
pub fn create_grep_tool(
    context: &ExecutionToolContext,
    options: Option<GrepToolOptions>,
) -> Arc<dyn AgentTool> {
    let _ = options; // forward-compat seam, no surface in v1
    Arc::new(GrepTool {
        schema: GrepTool::schema(),
        env: context.env().clone(),
    })
}

#[async_trait]
impl AgentTool for GrepTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "grep"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: GrepInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("grep input invalid: {e}")))?;

        let cancel = Some(&signal);
        let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1) as usize;
        let context_val = input.context.unwrap_or(0).min(u32::MAX) as usize;

        // Compile the regex. `literal` ⇒ escape; `ignore_case` ⇒ case-insensitive.
        let pattern = if input.literal.unwrap_or(false) {
            regex::escape(&input.pattern)
        } else {
            input.pattern.clone()
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(input.ignore_case.unwrap_or(false))
            .build()
            .map_err(|e| AgentError::Validation(format!("grep pattern invalid: {e}")))?;

        // Compile the optional glob filter. `has_separator` decides whether to
        // match the full relative path (`**/*.ts`) or just the basename (`*.ts`),
        // mirroring rg's basename-glob leniency.
        let glob: Option<(globset::GlobMatcher, bool)> = match input.glob.as_deref() {
            Some(g) if !g.is_empty() => {
                let compiled = globset::Glob::new(g)
                    .map_err(|e| AgentError::Validation(format!("grep glob invalid: {e}")))?
                    .compile_matcher();
                Some((compiled, g.contains('/')))
            }
            _ => None,
        };

        // Resolve the search path. A `.`/absent path resolves to the env cwd.
        let search_path =
            resolve_read_tool_path(&*self.env, input.path.as_deref().unwrap_or("."), cancel)
                .await
                .map_err(file_err_to_agent)?;

        // Determine dir vs file (following a symlinked root via canonical_path).
        let info = self
            .env
            .file_info(&search_path, cancel)
            .await
            .map_err(file_err_to_agent)?;
        let root_kind = if info.kind == FileKind::Symlink {
            match self.env.canonical_path(&search_path, cancel).await {
                Ok(canon) => match self.env.file_info(&canon.to_string_lossy(), cancel).await {
                    Ok(i) => i.kind,
                    Err(_) => info.kind, // treat as whatever lstat said
                },
                Err(_) => info.kind,
            }
        } else {
            info.kind
        };

        // Mutable collection state shared across the file-search helper.
        let mut matches: usize = 0;
        let mut match_limit_reached = false;
        let mut lines_truncated = false;
        let mut output_lines: Vec<String> = Vec::new();

        if root_kind == FileKind::Directory {
            // Iterative BFS walk (avoids async-recursion boxing). Sorted entries
            // for deterministic output order.
            let mut tree = self
                .env
                .search_tree(&search_path, cancel)
                .await
                .map_err(file_err_to_agent)?;
            let mut queue =
                VecDeque::from([(Path::new(&search_path).to_path_buf(), String::new())]);
            while let Some((abs_dir, rel_dir)) = queue.pop_front() {
                if matches >= effective_limit {
                    break;
                }
                check_cancel(&signal)?;
                let mut entries = if let Some(tree) = &mut tree {
                    tree.remove(&abs_dir).unwrap_or_default()
                } else {
                    match self.env.list_dir(&abs_dir.to_string_lossy(), cancel).await {
                        Ok(entries) => entries
                            .into_iter()
                            .map(|entry| (entry.path, entry.kind))
                            .collect(),
                        Err(_) => {
                            check_cancel(&signal)?;
                            continue;
                        }
                    }
                };
                entries.sort_by_cached_key(|(path, _)| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                });
                let mut prefetched = stream::iter(entries)
                    .map(|(path, kind)| {
                        let rel_dir = &rel_dir;
                        let glob = glob.as_ref();
                        async move {
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            let rel = if rel_dir.is_empty() {
                                name.into_owned()
                            } else {
                                format!("{rel_dir}/{name}")
                            };
                            let text = if kind == FileKind::File && matches_glob(glob, &path, &rel)
                            {
                                self.env
                                    .read_text_file(&path.to_string_lossy(), cancel)
                                    .await
                                    .ok()
                            } else {
                                None
                            };
                            (path, kind, rel, text)
                        }
                    })
                    .buffered(PREFETCH_FILES);
                while let Some((path, kind, rel, text)) = prefetched.next().await {
                    check_cancel(&signal)?;
                    if matches >= effective_limit {
                        match_limit_reached = true;
                        break;
                    }
                    if kind == FileKind::Directory
                        && path.file_name().is_some_and(|name| name != ".git")
                    {
                        queue.push_back((path, rel));
                        continue;
                    }
                    if let Some(text) = text {
                        search_text(
                            &text,
                            &re,
                            context_val,
                            effective_limit,
                            &rel,
                            &mut matches,
                            &mut match_limit_reached,
                            &mut lines_truncated,
                            &mut output_lines,
                        );
                    }
                }
            }
        } else if root_kind == FileKind::File {
            let path = Path::new(&search_path);
            let rel = path.file_name().map_or_else(
                || search_path.as_str().into(),
                |name| name.to_string_lossy(),
            );
            let text = if matches_glob(glob.as_ref(), path, &rel) {
                self.env.read_text_file(&search_path, cancel).await.ok()
            } else {
                None
            };
            if let Some(text) = text {
                search_text(
                    &text,
                    &re,
                    context_val,
                    effective_limit,
                    &rel,
                    &mut matches,
                    &mut match_limit_reached,
                    &mut lines_truncated,
                    &mut output_lines,
                );
            }
        } else {
            // Symlink root that didn't resolve to a file/dir (or not_supported).
            return Err(AgentError::Tool(format!(
                "Path is neither a file nor a directory: {search_path}"
            )));
        }

        check_cancel(&signal)?;
        if output_lines.is_empty() {
            return Ok(AgentToolResult {
                content: vec![TextContentOrImage::text("No matches found")],
                details: serde_json::Value::Null,
                ..Default::default()
            });
        }

        let raw_output = output_lines.join("\n");
        // Byte cap only — line cap disabled (match count already capped).
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: Some(usize::MAX),
                max_bytes: None,
            },
        );
        let was_truncated = truncation.truncated;
        let mut output = truncation.content.clone();
        let mut details = GrepToolDetails {
            truncation: None,
            match_limit_reached: None,
            lines_truncated: None,
        };
        let mut notices: Vec<String> = Vec::new();
        if match_limit_reached {
            notices.push(format!(
                "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                effective_limit * 2
            ));
            details.match_limit_reached = Some(effective_limit as u32);
        }
        if was_truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details.truncation = Some(truncation);
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
            details.lines_truncated = Some(true);
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let details_value = serde_json::to_value(&details).unwrap_or(serde_json::Value::Null);
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output)],
            details: details_value,
            ..Default::default()
        })
    }
}

fn check_cancel(signal: &CancellationToken) -> Result<(), AgentError> {
    if signal.is_cancelled() {
        return Err(file_err_to_agent(crate::error::FileError::aborted()));
    }
    Ok(())
}

fn matches_glob(glob: Option<&(globset::GlobMatcher, bool)>, path: &Path, rel: &str) -> bool {
    match glob {
        Some((matcher, true)) => matcher.is_match(rel),
        Some((matcher, false)) => matcher.is_match(path.file_name().unwrap_or(path.as_os_str())),
        None => true,
    }
}

/// Append match and context lines for one file, maintaining the global limit.
#[allow(clippy::too_many_arguments)]
fn search_text(
    text: &str,
    re: &regex::Regex,
    context_val: usize,
    effective_limit: usize,
    rel: &str,
    matches: &mut usize,
    match_limit_reached: &mut bool,
    lines_truncated: &mut bool,
    output_lines: &mut Vec<String>,
) {
    // Preserve CRLF and lone-CR handling without copying ordinary LF files.
    let normalized;
    let text = if text.contains('\r') {
        normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        normalized.as_str()
    } else {
        text
    };
    let context_lines = (context_val != 0).then(|| text.split('\n').collect::<Vec<_>>());

    for (idx, line) in text.split('\n').enumerate() {
        if *matches >= effective_limit {
            *match_limit_reached = true;
            return;
        }
        if !re.is_match(line) {
            continue;
        }
        let line_number = idx + 1; // 1-indexed.
        *matches += 1;

        if let Some(lines) = &context_lines {
            let start = line_number.saturating_sub(context_val).max(1);
            let end = line_number.saturating_add(context_val).min(lines.len());
            for current in start..=end {
                let out = truncate_line_default(lines[current - 1]);
                *lines_truncated |= out.was_truncated;
                let separator = if current == line_number { ':' } else { '-' };
                output_lines.push(format!("{rel}{separator}{current}{separator} {}", out.text));
            }
        } else {
            let out = truncate_line_default(line);
            *lines_truncated |= out.was_truncated;
            output_lines.push(format!("{rel}:{line_number}: {}", out.text));
        }
    }
}

/// Map a `FileError` into an `AgentError::Tool` (same shape as `read.rs`).
fn file_err_to_agent(e: crate::error::FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
