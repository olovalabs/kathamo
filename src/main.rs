use anyhow::Result;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext, Bounds, ClipboardEntry, Context,
    CursorStyle, Div, Edges, Entity, ExternalPaths, Hsla, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, ParentElement, Pixels,
    Point, Rems, Render, ScrollEvent, SharedString, StatefulInteractiveElement as _,
    StyleRefinement, Styled, StyledImage as _, Transformation, Window, WindowControlArea,
    WindowDecorations, WindowOptions, actions, deferred, div, img, percentage, px, relative, rems,
    rgba, size, svg,
};
use gpui_component::WindowExt as _;
use gpui_component::highlighter::HighlightTheme;
use gpui_component::input::{Input, InputEvent, InputState, RopeExt as _};
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::{ActiveTheme as _, Root, Sizable as _, TitleBar};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use terminal::{ColorPalette, ShellEvent, TerminalConfig, TerminalView};

mod agent_auth;
mod agent_bridge;
mod agent_rules;
mod assets;
mod attachments;
mod attention;
mod cli;
mod completion;
mod conversations;
mod diagnostics;
mod integrations;
#[cfg(unix)]
mod kathamo_compat;
mod logos;
mod omp_bridge;
pub mod provider;
mod paths;
mod settings;
mod shell_history;
#[path = "blocks/shell_integration.rs"]
mod shell_integration;
mod sound;
mod status;
#[cfg(unix)]
mod status_bus;
mod theme;
mod workspace_state;

use theme::{PaletteScheme, ThemePreference, ThemeTokens, load_palette};

/// Key context of the Kathamo input bar.
///
/// Tab has to be bound here rather than handled from `on_key_down`: the input
/// component binds `tab` to `IndentInline` and `gpui-component` binds it at the
/// `Root` context for focus traversal, which would otherwise consume the key
/// before the completion handler runs.
const KATHAMO_INPUT_CONTEXT: &str = "KathamoInput";

actions!(kathamo_input, [CompleteCommand, IgnoreKey]);

fn init_kathamo_input_bindings(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("tab", CompleteCommand, Some(KATHAMO_INPUT_CONTEXT)),
        // Keep Shift+Tab from running the Root context's focus traversal.
        gpui::KeyBinding::new("shift-tab", IgnoreKey, Some(KATHAMO_INPUT_CONTEXT)),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ActiveView {
    #[default]
    Workspace,
    Settings,
}

use assets::{CombinedAssets, MONO_FONT, load_embedded_fonts, sync_component_fonts};
use status::{SessionStatus, StatusInput};
pub const APP_ICON_PNG: &[u8] = include_bytes!("../assets/logo/kathamo.png");
/// Fixed width of the navigation sidebar (Kathamo style).
pub const SIDEBAR_WIDTH: Pixels = px(260.0);

#[cfg(unix)]
static STATUS_BUS: std::sync::LazyLock<Option<status_bus::BusIdentity>> =
    std::sync::LazyLock::new(|| status_bus::start().ok());
#[cfg(unix)]
static LAST_STATUS_SNAPSHOT_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn status_snapshot_due() -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let previous = LAST_STATUS_SNAPSHOT_MS.load(Ordering::Relaxed);
    now.saturating_sub(previous) >= 1000
        && LAST_STATUS_SNAPSHOT_MS
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

struct SharedWriter {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    read_only: Arc<AtomicBool>,
}

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.read_only.load(Ordering::Relaxed) {
            return Ok(buf.len());
        }
        self.writer.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.read_only.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.writer.lock().flush()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ChatRole {
    User,
    Assistant,
    Thinking,
    Status,
    Tool,
    Error,
}

#[derive(Clone)]
struct ChatMessage {
    role: ChatRole,
    text: String,
    tool: Option<ChatTool>,
    expanded: bool,
    /// Assistant text that is still being appended to. Streaming text renders
    /// settled markdown blocks eagerly and the live tail as plain text so each
    /// token does not re-parse and re-shape the whole message.
    streaming: bool,
}

#[derive(Clone)]
struct ChatTool {
    id: Option<String>,
    name: String,
    input: String,
    output: String,
    state: ToolState,
    expanded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ToolState {
    Running,
    Completed,
    Failed,
    Unknown,
}
impl ChatMessage {
    fn new(role: ChatRole, text: String) -> Self {
        Self {
            role,
            text,
            tool: None,
            expanded: true,
            streaming: false,
        }
    }
}

const TOOL_TEXT_PREVIEW_LIMIT: usize = 600;

fn fence_open(line: &str) -> Option<(u8, usize)> {
    let line = line.trim_end_matches(['\r', '\n']);
    let marker = *line.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = line.bytes().take_while(|b| *b == marker).count();
    if len < 3 {
        return None;
    }
    if marker == b'`' && line[len..].contains('`') {
        return None;
    }
    Some((marker, len))
}

fn fence_close(line: &str, marker: u8, len: usize) -> bool {
    let line = line.trim_end_matches(['\r', '\n']).trim_end();
    let count = line.bytes().take_while(|b| *b == marker).count();
    count >= len && line[count..].trim().is_empty()
}

fn list_marker_len(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    match bytes.first()? {
        b'-' | b'*' | b'+' if bytes.get(1) == Some(&b' ') => Some(1),
        b'0'..=b'9' => {
            let mut digits = 0;
            while bytes.get(digits).is_some_and(u8::is_ascii_digit) {
                digits += 1;
            }
            if (1..=9).contains(&digits)
                && matches!(bytes.get(digits), Some(b'.') | Some(b')'))
                && bytes.get(digits + 1) == Some(&b' ')
            {
                Some(digits + 1)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Split a streaming message into settled markdown and the single block that is
/// still being written (Codex's stable/tail partitioning).
///
/// Everything before the boundary can be parsed once and cached. The tail is
/// the only part re-parsed per token, so streaming stays O(tail) instead of
/// O(document). Blank lines inside fences are not boundaries, and a list that
/// is still being written stays whole in the tail so its numbering and nesting
/// keep their context.
fn split_streaming_markdown(text: &str) -> (&str, &str) {
    let mut fence: Option<(u8, usize)> = None;
    let mut boundary: Option<usize> = None;
    let mut last_blank: Option<usize> = None;
    let mut in_list = false;
    let mut list_start: Option<usize> = None;
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();

        let trimmed = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - trimmed.len();

        if let Some((marker, len)) = fence {
            if fence_close(trimmed, marker, len) {
                fence = None;
            }
            continue;
        }

        if indent <= 3 {
            if let Some(opened) = fence_open(trimmed) {
                in_list = false;
                list_start = None;
                boundary = last_blank.or(boundary);
                fence = Some(opened);
                continue;
            }
        }

        if line.trim().is_empty() {
            last_blank = Some(start);
            if !in_list {
                boundary = Some(start);
            }
            continue;
        }

        if indent <= 3 && list_marker_len(trimmed).is_some() {
            if !in_list {
                in_list = true;
                list_start = Some(boundary.or(last_blank).unwrap_or(0));
            }
        } else if in_list && indent >= 2 {
            // Indented continuation of the current list item.
        } else {
            in_list = false;
            list_start = None;
            boundary = last_blank.or(boundary);
        }
    }

    let at = if in_list { list_start } else { boundary };
    match at {
        Some(at) => (&text[..at], text[at..].trim_start_matches(['\r', '\n'])),
        None => ("", text),
    }
}

/// Unicode-safe clipping: never slices through a multi-byte character.
fn clip_tool_text(text: &str, limit: usize) -> (String, bool) {
    let mut chars = text.chars();
    let mut shown: String = chars.by_ref().take(limit).collect();
    let truncated = chars.next().is_some();
    if truncated {
        shown.push('…');
    }
    (shown, truncated)
}

fn format_tool_summary(_name: &str, input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return String::new();
    }
    let clip = |text: &str| clip_tool_text(text, 60).0;
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(input) {
        if let Some(map) = value.as_object() {
            if let Some(cmd) = map.get("command").and_then(|v| v.as_str()) {
                let first_line = cmd.lines().next().unwrap_or("").trim();
                return format!("$ {}", clip(first_line));
            }
            if let Some(path) = map.get("path").and_then(|v| v.as_str()) {
                if let Some(pattern) = map.get("pattern").and_then(|v| v.as_str()) {
                    return format!("{path} · {pattern}");
                }
                return path.to_string();
            }
            if let Some(pattern) = map.get("pattern").and_then(|v| v.as_str()) {
                return format!("/{pattern}/");
            }
            if let Some(query) = map.get("query").and_then(|v| v.as_str()) {
                return format!("\"{query}\"");
            }
            if let Some(url) = map.get("url").and_then(|v| v.as_str()) {
                return url.to_string();
            }
            let mut parts = Vec::new();
            for (k, v) in map.iter().take(2) {
                let val_str = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    _ => serde_json::to_string(v).unwrap_or_default(),
                };
                parts.push(format!("{k}: {val_str}"));
            }
            if !parts.is_empty() {
                return clip(&parts.join(", "));
            }
        }
    }
    clip(input.lines().next().unwrap_or("").trim())
}

/// Markdown body shared by user and assistant chat messages.
///
/// Mirrors the Wake reading pane: GFM tables/quotes/dividers come from the
/// component, headings scale relative to body text, and code blocks get a
/// raised surface plus a language label and copy action.
fn chat_markdown(
    id: SharedString,
    text: impl Into<SharedString>,
    base: Pixels,
    paragraph_gap: Rems,
    tokens: ThemeTokens,
    window: &mut Window,
    cx: &mut App,
) -> TextView {
    let dark = cx.theme().is_dark();
    let themed_id: SharedString = format!("{id}-{}", if dark { "dark" } else { "light" }).into();

    TextView::markdown(themed_id, text, window, cx)
        .style(
            TextViewStyle {
                heading_base_font_size: base,
                paragraph_gap,
                is_dark: dark,
                highlight_theme: if dark {
                    HighlightTheme::default_dark()
                } else {
                    HighlightTheme::default_light()
                },
                ..Default::default()
            }
            .heading_font_size(|level, base| match level {
                1 => base * 1.45,
                2 => base * 1.28,
                3 => base * 1.14,
                4 => base * 1.05,
                _ => base,
            })
            .code_block(
                StyleRefinement::default()
                    .bg(tokens.card_hover)
                    .border_1()
                    .border_color(tokens.border_subtle)
                    .rounded(px(8.0))
                    .px(px(16.0))
                    .py(px(12.0)),
            ),
        )
        .code_block_actions(move |block, _window, _cx| {
            let code = block.code();
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(6.0))
                .text_size(px(11.0))
                .text_color(tokens.text_muted)
                .when_some(block.lang(), |actions, lang| actions.child(lang))
                .child(
                    div()
                        .id("chat-code-copy")
                        .size(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor(CursorStyle::PointingHand)
                        .hover(move |style| style.bg(tokens.sidebar_item_hover))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                code.to_string(),
                            ));
                        })
                        .child(
                            svg()
                                .path("icons/copy.svg")
                                .size(px(12.0))
                                .text_color(tokens.text_muted),
                        ),
                )
                .into_any_element()
        })
        .selectable(true)
        .w_full()
        .text_size(base)
        .text_color(tokens.text_primary)
}

/// One collapsed tool-call card, matching Wake's tool cluster:
/// chevron + tool name + inline argument preview + state badge on the header,
/// then up to 600 characters of Input/Output in monospace panels.
fn chat_tool_card(
    pane_id: usize,
    index: usize,
    tool: &ChatTool,
    tokens: ThemeTokens,
    on_toggle: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let expanded = tool.expanded;
    let summary = format_tool_summary(&tool.name, &tool.input);
    let failed = tool.state == ToolState::Failed;
    let mono = MONO_FONT;

    let mut card = div()
        .w_full()
        .min_w_0()
        .rounded(px(8.0))
        .border_1()
        .border_color(tokens.border_subtle)
        .bg(tokens.card_hover)
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .id(SharedString::from(format!(
                    "tool-card-header-{pane_id}-{index}"
                )))
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(12.0))
                .py(px(8.0))
                .cursor(CursorStyle::PointingHand)
                .text_size(px(11.5))
                .text_color(tokens.text_muted)
                .hover(move |style| style.text_color(tokens.text_primary))
                .on_mouse_down(MouseButton::Left, on_toggle)
                .child(
                    svg()
                        .path(if expanded {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        })
                        .size(px(11.0))
                        .flex_shrink_0()
                        .text_color(tokens.text_muted),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(tokens.text_primary)
                        .child(tool.name.clone()),
                )
                .when(!summary.is_empty(), |header| {
                    header.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(mono)
                            .child(summary),
                    )
                })
                .child(div().flex_1())
                .when(failed, |header| {
                    header.child(
                        div()
                            .flex_shrink_0()
                            .px(px(7.0))
                            .rounded(px(4.0))
                            .text_size(px(11.0))
                            .text_color(tokens.block_failure)
                            .bg(tokens.block_failure.opacity(0.12))
                            .child("Failed"),
                    )
                })
                .when(tool.state == ToolState::Running, |header| {
                    header.child(
                        div()
                            .flex_shrink_0()
                            .px(px(7.0))
                            .rounded(px(4.0))
                            .text_size(px(11.0))
                            .text_color(tokens.block_warning)
                            .bg(tokens.block_warning.opacity(0.12))
                            .child("Running…"),
                    )
                }),
        );

    if expanded {
        let mut body = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .border_t_1()
            .border_color(tokens.border_subtle)
            .px(px(12.0))
            .py(px(10.0));

        let display_input = serde_json::from_str::<serde_json::Value>(&tool.input)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| tool.input.clone());
        if !display_input.trim().is_empty() {
            body = body.child(chat_tool_section(
                SharedString::from(format!("copy-tool-input-{pane_id}-{index}")),
                "Input",
                &display_input,
                false,
                tokens,
            ));
        }

        if !tool.output.trim().is_empty() {
            body = body.child(chat_tool_section(
                SharedString::from(format!("copy-tool-{pane_id}-{index}")),
                "Output",
                &tool.output,
                failed,
                tokens,
            ));
        } else if tool.state == ToolState::Running {
            body = body.child(
                div()
                    .text_size(px(11.5))
                    .text_color(tokens.text_muted)
                    .child("Running…"),
            );
        }

        card = card.child(body);
    }

    card
}

fn chat_tool_section(
    copy_id: SharedString,
    label: &'static str,
    body: &str,
    is_error: bool,
    tokens: ThemeTokens,
) -> Div {
    let (shown, truncated) = clip_tool_text(body, TOOL_TEXT_PREVIEW_LIMIT);
    let full = body.to_string();
    let copy_tooltip: SharedString = format!("Copy full {}", label.to_lowercase()).into();

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(11.0))
                .text_color(if is_error {
                    tokens.block_failure
                } else {
                    tokens.text_muted
                })
                .child(label)
                .when(truncated, |header| {
                    header.child(
                        div()
                            .text_color(tokens.text_muted)
                            .child(format!("First {} characters", TOOL_TEXT_PREVIEW_LIMIT)),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .id(copy_id)
                        .size(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor(CursorStyle::PointingHand)
                        .text_color(tokens.text_muted)
                        .hover(move |style| {
                            style
                                .bg(tokens.sidebar_item_hover)
                                .text_color(tokens.text_primary)
                        })
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(copy_tooltip.clone())
                                .build(window, cx)
                        })
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(full.clone()));
                        })
                        .child(
                            svg()
                                .path("icons/copy.svg")
                                .size(px(12.0))
                                .text_color(tokens.text_muted),
                        ),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(5.0))
                .bg(if is_error {
                    tokens.block_failure.opacity(0.08)
                } else {
                    tokens.card_bg
                })
                .whitespace_normal()
                .text_size(px(11.5))
                .line_height(relative(1.6))
                .font_family(MONO_FONT)
                .text_color(if is_error {
                    tokens.block_failure
                } else {
                    tokens.text_secondary
                })
                .child(shown),
        )
}

fn update_chat_tool(
    messages: &mut Vec<ChatMessage>,
    id: Option<String>,
    name: String,
    output: String,
    state: ToolState,
) {
    // An absent ID cannot distinguish concurrent calls with the same name.
    // Associate those events with the oldest matching running call.
    let existing = messages
        .iter_mut()
        .rev()
        .take_while(|message| message.role != ChatRole::User)
        .filter_map(|message| message.tool.as_mut())
        .filter(|tool| tool.state == ToolState::Running)
        .filter(|tool| match id.as_ref() {
            Some(id) => tool.id.as_ref() == Some(id),
            None => tool.id.is_none() && tool.name == name,
        })
        .last();
    if let Some(tool) = existing {
        tool.output = output;
        tool.state = state;
    } else {
        messages.push(ChatMessage {
            role: ChatRole::Tool,
            text: String::new(),
            tool: Some(ChatTool {
                id,
                name,
                input: String::new(),
                output,
                state,
                expanded: false,
            }),
            expanded: false,
            streaming: false,
        });
    }
}

fn agent_model_label(model: &agent_auth::AgentModel) -> String {
    format!("{} · {}", model.provider, model.id)
}

/// Turn a pre-prompt agent failure into a short, actionable default-screen note.
fn agent_startup_notice(error: &str) -> (String, String) {
    let lower = error.to_ascii_lowercase();
    let credentials = [
        "api key",
        "apikey",
        "authenticat",
        "credential",
        "login",
        "sign in",
        "unauthorized",
        "401",
        "oauth",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let message = if credentials {
        "This model is not signed in yet."
    } else {
        "The agent could not start."
    };
    let hint = if credentials {
        " Run /auth to connect a provider, or choose another model with /model."
    } else {
        " Check the agent installation, then try again."
    };
    let detail = clip_tool_text(error.lines().next().unwrap_or("").trim(), 180).0;
    (format!("{message}{hint}"), detail)
}

/// Assistant text stops streaming whenever the agent moves on to thinking,
/// activity, a tool call, an error, or the end of the turn. The next text
/// event starts a fresh streaming message, so completed text gets the full
/// markdown render again.
fn finish_chat_streaming(messages: &mut [ChatMessage]) {
    for message in messages.iter_mut() {
        if message.streaming {
            message.streaming = false;
        }
    }
}

fn finish_chat_tools(messages: &mut [ChatMessage]) {
    for tool in messages
        .iter_mut()
        .rev()
        .take_while(|message| message.role != ChatRole::User)
        .filter_map(|message| message.tool.as_mut())
        .filter(|tool| tool.state == ToolState::Running)
    {
        tool.state = ToolState::Unknown;
    }
}

fn push_chat_event(messages: &mut Vec<ChatMessage>, event: agent_bridge::AgentEvent) {
    let (role, text) = match event {
        agent_bridge::AgentEvent::ToolStart { id, name, input } => {
            messages.push(ChatMessage {
                role: ChatRole::Tool,
                text: String::new(),
                tool: Some(ChatTool {
                    id,
                    name,
                    input,
                    output: String::new(),
                    state: ToolState::Running,
                    expanded: false,
                }),
                expanded: false,
                streaming: false,
            });
            finish_chat_streaming(messages);
            return;
        }
        agent_bridge::AgentEvent::ToolUpdate { id, name, output } => {
            update_chat_tool(messages, id, name, output, ToolState::Running);
            finish_chat_streaming(messages);
            return;
        }
        agent_bridge::AgentEvent::ToolEnd {
            id,
            name,
            output,
            failed,
        } => {
            update_chat_tool(
                messages,
                id,
                name,
                output,
                if failed {
                    ToolState::Failed
                } else {
                    ToolState::Completed
                },
            );
            finish_chat_streaming(messages);
            return;
        }
        agent_bridge::AgentEvent::Text(text) => (ChatRole::Assistant, text),
        agent_bridge::AgentEvent::Thinking(text) => (ChatRole::Thinking, text),
        agent_bridge::AgentEvent::Status(text) => (ChatRole::Status, text),
        agent_bridge::AgentEvent::Tool(text) => (ChatRole::Tool, text),
        agent_bridge::AgentEvent::Error(text) => (ChatRole::Error, text),
        agent_bridge::AgentEvent::Done | agent_bridge::AgentEvent::Closed => return,
    };
    if role == ChatRole::Assistant {
        for message in messages
            .iter_mut()
            .rev()
            .take_while(|m| m.role != ChatRole::User)
        {
            if message.role == ChatRole::Thinking {
                message.expanded = false;
            }
        }
    } else {
        finish_chat_streaming(messages);
    }
    if matches!(role, ChatRole::Assistant | ChatRole::Thinking) {
        if let Some(last) = messages.last_mut().filter(|message| message.role == role) {
            last.text.push_str(&text);
            if role == ChatRole::Assistant {
                last.streaming = true;
            }
            return;
        }
    }
    let mut message = ChatMessage::new(role, text);
    if role == ChatRole::Thinking {
        message.expanded = false;
    }
    if role == ChatRole::Assistant {
        message.streaming = true;
    }
    messages.push(message);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentDialogPage {
    Auth,
    Models,
}

struct AuthModal {
    page: AgentDialogPage,
    focus: gpui::FocusHandle,
    cwd: PathBuf,
    providers: Vec<agent_auth::Provider>,
    discovery: Option<std::sync::mpsc::Receiver<Result<Vec<agent_auth::Provider>, String>>>,
    models: Vec<agent_auth::AgentModel>,
    model_discovery: Option<std::sync::mpsc::Receiver<Result<Vec<agent_auth::AgentModel>, String>>>,
    model_error: Option<String>,
    provider_input: Entity<InputState>,
    input: Entity<InputState>,
    login: Option<agent_auth::Login>,
    selected: Option<String>,
    output: String,
    error: Option<String>,
}

impl AuthModal {
    fn discover(&mut self) {
        self.error = None;
        self.model_error = None;
        let (sender, receiver) = std::sync::mpsc::channel();
        let cwd = self.cwd.clone();
        std::thread::spawn(move || {
            let _ = sender.send(agent_auth::providers(&cwd));
        });
        self.discovery = Some(receiver);
        let (sender, receiver) = std::sync::mpsc::channel();
        let cwd = self.cwd.clone();
        std::thread::spawn(move || {
            let _ = sender.send(agent_auth::available_models(&cwd));
        });
        self.model_discovery = Some(receiver);
    }
}

enum ModelBridge {
    Rpi(agent_bridge::AgentBridge),
    Omp(omp_bridge::OmpBridge),
}

impl ModelBridge {
    fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        match self {
            Self::Rpi(bridge) => bridge.send_prompt(prompt),
            Self::Omp(bridge) => bridge.send_prompt(prompt),
        }
    }
    fn cancel(&mut self) -> Result<(), String> {
        match self {
            Self::Rpi(bridge) => bridge.cancel(),
            Self::Omp(bridge) => bridge.cancel(),
        }
    }
    fn poll(&mut self) -> Vec<agent_bridge::AgentEvent> {
        match self {
            Self::Rpi(bridge) => bridge.poll(),
            Self::Omp(bridge) => bridge.poll(),
        }
    }
    fn set_thinking_level(&mut self, level: &str) -> Result<(), String> {
        match self {
            Self::Omp(bridge) => bridge.set_thinking_level(level),
            Self::Rpi(_) => Ok(()),
        }
    }
}

#[derive(Default)]
struct ModelChat {
    bridge: Option<ModelBridge>,
    messages: Vec<ChatMessage>,
    running: bool,
    auth_refresh_pending: bool,
    rpi_session_id: Option<String>,
    conversation: Option<conversations::Log>,
    /// A failure reported before the first prompt; shown on the default screen
    /// instead of as a transcript message.
    startup_error: Option<String>,
}

struct PaneChat {
    active: bool,
    bridge: Option<agent_bridge::AgentBridge>,
    cwd: Option<PathBuf>,
    messages: Vec<ChatMessage>,
    running: bool,
    terminal_draft: String,
    scroll: gpui::ScrollHandle,
    agent_draft: String,
    auth_refresh_pending: bool,
    auth: Option<AuthModal>,
    selected_model: Option<agent_auth::AgentModel>,
    models: HashMap<agent_auth::AgentModel, ModelChat>,
    fast_mode: bool,
    pub autoscroll: bool,
    rpi_session_id: Option<String>,
    conversation: Option<conversations::Log>,
    startup_error: Option<String>,
}

impl Default for PaneChat {
    fn default() -> Self {
        Self {
            active: false,
            bridge: None,
            cwd: None,
            messages: Vec::new(),
            running: false,
            terminal_draft: String::new(),
            scroll: gpui::ScrollHandle::new(),
            agent_draft: String::new(),
            auth_refresh_pending: false,
            auth: None,
            selected_model: None,
            models: HashMap::new(),
            fast_mode: true,
            autoscroll: true,
            rpi_session_id: None,
            conversation: None,
            startup_error: None,
        }
    }
}

impl PaneChat {
    fn visible_messages(&self) -> &[ChatMessage] {
        match self.selected_model.as_ref() {
            Some(model) => self
                .models
                .get(model)
                .map(|chat| chat.messages.as_slice())
                .unwrap_or(&[]),
            None => &self.messages,
        }
    }
    fn visible_messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        match self.selected_model.as_ref() {
            Some(model) => &mut self.models.entry(model.clone()).or_default().messages,
            None => &mut self.messages,
        }
    }

    fn visible_running(&self) -> bool {
        match self.selected_model.as_ref() {
            Some(model) => self.models.get(model).is_some_and(|chat| chat.running),
            None => self.running,
        }
    }

    fn visible_startup_error(&self) -> Option<&str> {
        match self.selected_model.as_ref() {
            Some(model) => self
                .models
                .get(model)
                .and_then(|chat| chat.startup_error.as_deref()),
            None => self.startup_error.as_deref(),
        }
    }

    fn visible_conversation(&mut self) -> Option<&mut conversations::Log> {
        match self.selected_model.as_ref() {
            Some(model) => self
                .models
                .get_mut(model)
                .and_then(|chat| chat.conversation.as_mut()),
            None => self.conversation.as_mut(),
        }
    }
}

/// One terminal inside a session's screen. A session (a sidebar tab) owns a
/// split tree of panes; each pane is a full, independent terminal whose PTY
/// keeps running while another session's screen is shown.
pub struct TerminalPane {
    pub id: usize,
    /// Stable identity across restarts, used for scrollback and conversation files.
    pub uid: String,
    /// Last working directory reported by shell integration (OSC 7 / 7502).
    pub cwd: Option<PathBuf>,
    pub generation: String,
    pub native_session_id: Option<String>,
    pub native_session_reporter: Option<String>,
    pub native_turn_id: Option<String>,
    pub native_sequence: HashMap<String, u64>,
    pub native_reporter_generation: HashMap<String, String>,
    pub pending_approvals: std::collections::HashSet<String>,
    pub native_status: Option<SessionStatus>,
    pub native_acknowledged: bool,
    pub osc_report_active: bool,
    pub terminal: Entity<TerminalView>,
    pub status: SessionStatus,
    pub detection: status::DetectionResult,
    pub title: String,
    /// Title as shown in the pane header (truncated to 160 chars), cached so
    /// the render pass does not re-scan and re-allocate it every frame.
    pub display_title: SharedString,
    pub pid: Option<u32>,
    /// Reused scrape buffer: the poller reads the screen into this instead of
    /// allocating a fresh `Vec<String>` every 500 ms per pane.
    pub screen_scratch: Vec<String>,
    /// (screen revision, focused, exited, foreground snapshot) from the last
    /// poll tick. The revision lets an idle pane skip rebuilding its screen
    /// strings entirely; the process snapshot is compared by `Arc` identity,
    /// which is O(1) because the `/proc` cache hands back the same allocation
    /// while the foreground process group is unchanged.
    pub poll_cache: Option<(u64, bool, bool, Arc<[String]>)>,
    /// Screen fingerprint at the moment the user last looked at a finished or
    /// failed terminal. While the screen still matches, the sticky Done/Error
    /// badge stays acknowledged (Idle); any new output invalidates it and lets
    /// a fresh Done/Error be raised.
    pub ack_screen: Option<u64>,
    /// A recognized agent CLI was the foreground process on the previous tick.
    /// Its disappearance means the agent finished (Done), like kathamo.
    pub agent_active: bool,
    pub detector: status::Detector,
    /// Kathamo-style seen/recency bookkeeping for this pane's finalized status.
    pub attention: attention::Attention,
    /// The agent's own name from its OSC 7501 report (`app`), when it reports one.
    pub app: Option<SharedString>,
    /// Sticky: this pane hosted a recognized agent. Cleared only when recreated.
    pub is_agent: bool,
    /// Workers reported by this pane's agent, keyed by their `agent_id`.
    pub sub_agents: HashMap<String, attention::SubAgent>,
    chat: PaneChat,
    pub input_state: Option<Entity<InputState>>,
    /// Text typed before the last quit, applied when the input is first built.
    pub restore_draft: Option<String>,
    /// Drops the `InputEvent::Change` subscription that keeps ghost text and
    /// the completion menu in sync with what is typed.
    pub input_subscription: Option<gpui::Subscription>,
    /// Shell history loaded from disk, shared by every pane.
    pub shell_history: Arc<Vec<String>>,
    /// Commands executed from this pane's input bar, newest last.
    pub command_history: Vec<String>,
    pub history_index: Option<usize>,
    pub pending_draft: String,
    /// Open completion menu (Kathamo-style suggestions above the input).
    pub completion_menu: Option<CompletionMenuState>,
    pub bottom_input_visible: bool,
    pub plus_menu_open: bool,
    /// Images staged in the input bar; dropped, pasted, or picked. They are
    /// sent to the agent as file paths when the prompt is submitted.
    pub attachments: Vec<attachments::AttachedImage>,
    /// True while the shell reports a command owns the terminal.
    pub command_running: bool,
    /// Keeps the terminal's `ShellEvent` subscription alive for this pane.
    pub terminal_subscription: Option<gpui::Subscription>,
}

/// One visible completion popup for a pane.
pub struct CompletionMenuState {
    /// Byte range in the input value that accepting replaces.
    pub start: usize,
    pub end: usize,
    /// The token these candidates were computed for, so typing resets the
    /// highlight instead of carrying it into a different list.
    pub token: String,
    pub items: Vec<completion::CompletionItem>,
    pub selected: usize,
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        #[cfg(unix)]
        kathamo_compat::unregister(&self.generation);
    }
}

impl TerminalPane {
    pub fn new(
        id: usize,
        generation: String,
        terminal: Entity<TerminalView>,
        title: SharedString,
        display_title: SharedString,
        pid: Option<u32>,
    ) -> Self {
        Self {
            id,
            uid: paths::uid("p"),
            cwd: None,
            generation,
            native_session_id: None,
            native_session_reporter: None,
            native_turn_id: None,
            native_sequence: HashMap::new(),
            native_reporter_generation: HashMap::new(),
            pending_approvals: std::collections::HashSet::new(),
            native_status: None,
            native_acknowledged: false,
            osc_report_active: false,
            terminal,
            status: SessionStatus::Idle,
            detection: status::DetectionResult::new(
                SessionStatus::Idle,
                status::EvidenceSource::NoSignal,
                status::Confidence::None,
            ),
            title: title.to_string(),
            display_title,
            pid,
            screen_scratch: Vec::new(),
            poll_cache: None,
            ack_screen: None,
            agent_active: false,
            detector: status::Detector::default(),
            attention: Default::default(),
            app: None,
            is_agent: false,
            sub_agents: HashMap::new(),
            chat: PaneChat::default(),
            input_state: None,
            restore_draft: None,
            input_subscription: None,
            shell_history: shell_history::cached(),
            command_history: Vec::new(),
            history_index: None,
            pending_draft: String::new(),
            completion_menu: None,
            bottom_input_visible: true,
            plus_menu_open: false,
            attachments: Vec::new(),
            command_running: false,
            terminal_subscription: None,
        }
    }

    /// Show the editor at the shell prompt; running commands and alternate
    /// screen programs receive input directly through the terminal.
    pub fn should_show_bottom_input(&self, cx: &App) -> bool {
        if self.chat.active {
            return true;
        }
        if !self.bottom_input_visible {
            return false;
        }

        // While a command owns stdin, the editor is hidden and raw input goes
        // straight to the PTY (sudo, ssh, read, interactive CLIs).
        if self.command_running {
            return false;
        }

        // Alternate screen mode (vim, nano, htop, less, etc.) always hides the bottom bar
        if self.terminal.read(cx).is_alt_screen() {
            return false;
        }

        true
    }
}

/// One sidebar tab: an isolated screen with its own split-layout tree of
/// panes. Switching tabs only swaps which screen is rendered — the panes of
/// other sessions keep running in the background.
pub struct Session {
    pub id: usize,
    /// Stable identity across restarts.
    pub uid: String,
    /// OSC title of the focus pane (synced in the poller / title callback).
    pub title: SharedString,
    /// Title as shown in the sidebar (truncated), cached.
    pub display_title: SharedString,
    /// Path of the AI agent / command logo shown in the sidebar.
    pub logo: SharedString,
    pub pane_tree: Option<PaneNode>,
    pub maximized_pane_id: Option<usize>,
    pub panes: Vec<TerminalPane>,
    /// Highest-ranked pane status, for the sidebar badge and title bar.
    pub status: SessionStatus,
}

/// Status poll period. Process status cannot be evented portably across the
/// platforms this app supports, so this bounded timer is the change detector;
/// the work it triggers is small (`/proc` reads now happen off the UI thread).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
#[cfg(unix)]
static FANOUT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn latch_completion(previous: SessionStatus, next: SessionStatus) -> SessionStatus {
    // Completion persistence is owned by the lifecycle reducer: idle/focus
    // observations cannot clear it, while fresh work always can.
    if matches!(previous, SessionStatus::Done | SessionStatus::Error)
        && matches!(
            next,
            SessionStatus::Idle | SessionStatus::Active | SessionStatus::Unknown
        )
    {
        previous
    } else {
        next
    }
}

#[cfg(test)]
#[test]
fn completed_status_survives_idle_and_focus_but_new_work_replaces_it() {
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Idle),
        SessionStatus::Done
    );
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Active),
        SessionStatus::Done
    );
    assert_eq!(
        latch_completion(SessionStatus::Error, SessionStatus::Unknown),
        SessionStatus::Error
    );
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Working),
        SessionStatus::Working
    );
    assert_eq!(
        latch_completion(SessionStatus::Done, SessionStatus::Blocked),
        SessionStatus::Blocked
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneNode {
    Leaf(usize),
    Split {
        id: usize,
        axis: gpui::Axis,
        left: Box<PaneNode>,
        right: Box<PaneNode>,
    },
}

impl PaneNode {
    pub fn contains(&self, id: usize) -> bool {
        match self {
            PaneNode::Leaf(leaf_id) => *leaf_id == id,
            PaneNode::Split { left, right, .. } => left.contains(id) || right.contains(id),
        }
    }

    pub fn split(
        &mut self,
        target_id: usize,
        new_id: usize,
        split_id: usize,
        axis: gpui::Axis,
    ) -> bool {
        match self {
            PaneNode::Leaf(id) => {
                if *id == target_id {
                    *self = PaneNode::Split {
                        id: split_id,
                        axis,
                        left: Box::new(PaneNode::Leaf(target_id)),
                        right: Box::new(PaneNode::Leaf(new_id)),
                    };
                    true
                } else {
                    false
                }
            }
            PaneNode::Split { left, right, .. } => {
                left.split(target_id, new_id, split_id, axis)
                    || right.split(target_id, new_id, split_id, axis)
            }
        }
    }

    pub fn remove(self, target_id: usize) -> Option<PaneNode> {
        match self {
            PaneNode::Leaf(id) => {
                if id == target_id {
                    None
                } else {
                    Some(PaneNode::Leaf(id))
                }
            }
            PaneNode::Split {
                id,
                axis,
                left,
                right,
            } => {
                let left = left.remove(target_id);
                let right = right.remove(target_id);
                match (left, right) {
                    (Some(l), Some(r)) => Some(PaneNode::Split {
                        id,
                        axis,
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                    (Some(l), None) => Some(l),
                    (None, Some(r)) => Some(r),
                    (None, None) => None,
                }
            }
        }
    }

    pub fn first_leaf(&self) -> usize {
        match self {
            PaneNode::Leaf(id) => *id,
            PaneNode::Split { left, .. } => left.first_leaf(),
        }
    }
}

pub struct Project {
    pub id: usize,
    /// Shared (cheap to clone) because the render pass copies it every frame.
    pub name: SharedString,
    pub path: PathBuf,
    pub sessions: Vec<Session>,
    pub active_session_idx: usize,
    /// Whether this project's session list is expanded in the sidebar. Every
    /// project keeps its own state, so several open projects can be browsed
    /// independently without switching away from the one being worked in.
    pub expanded: bool,
}

struct AppState {
    projects: Vec<Project>,
    active_project_idx: usize,
    next_id: usize,
    palette: ColorPalette,
    /// O(1) session lookup by id: session id -> (project idx, session idx).
    session_index: HashMap<usize, (usize, usize)>,
    /// O(1) pane lookup by id: pane id -> (project idx, session idx, pane idx).
    pane_index: HashMap<usize, (usize, usize, usize)>,
    /// Pane that currently owns keyboard focus, tracked every render frame
    /// and consumed by the status poller (Active state, sticky-state clearing).
    focused_pane_id: Option<usize>,
    pub active_view: ActiveView,
    pub theme_preference: ThemePreference,
    /// Terminal color palette / block chrome (Warp by default).
    pub palette_scheme: PaletteScheme,
    default_agent_model: Option<agent_auth::AgentModel>,
    /// OMP's own configured default, used when no Kathamo default is set.
    fallback_agent_model: Option<agent_auth::AgentModel>,
    pub sidebar_visible: bool,
    integration_panel_visible: bool,
    integration_feedback: SharedString,
    pub sidebar_width: Pixels,
    drag_start_pos: Option<Point<Pixels>>,
    persistence_path: Option<PathBuf>,
    save_generation: Arc<AtomicU64>,
    startup_loaded: bool,
    quit_subscription: Option<gpui::Subscription>,
    /// Whether the OS considers this window focused, mirrored from the render
    /// pass. "Looking" requires it: a visible-but-backgrounded window hasn't
    /// been seen.
    window_active: bool,
    /// The first poll tick only primes the attention model; it never alerts.
    status_primed: bool,
    /// Alerts to drain on the next render, where a window is available to
    /// push notifications and play sounds.
    pending_alerts: Vec<attention::AppAlert>,
    /// Monotonic counter for "most recent status change" ordering.
    attention_seq: u64,
    /// Currently open project dropdown menu: Some(project_id).
    project_menu: Option<usize>,
    renaming_project: Option<RenamingProject>,
    pub terminal_context_menu: Option<TerminalContextMenu>,
    /// Ctrl+R reverse history search palette, if open.
    history_search: Option<HistorySearchState>,
    /// Lightbox for an attached image, if open.
    image_viewer: Option<ImageViewerState>,
    /// User-editable settings (also mirrored to `settings.json`).
    settings: settings::AppSettings,
    /// Set whenever durable state changes; the 30 s autosave drains it.
    dirty: bool,
    /// Last known window geometry, captured on close and while saving.
    last_window_bounds: Option<settings::SavedWindow>,
    /// False when launched with `KATHAMO_DISABLE_RESTORE`; the app then never
    /// writes to the saved state (read-only troubleshooting mode).
    persistence_enabled: bool,
    /// Two-step confirmation state for "clear saved sessions".
    clear_data_confirm: bool,
}

/// Full-window preview of one attached image.
struct ImageViewerState {
    path: PathBuf,
    name: String,
    dimensions: Option<(u32, u32)>,
}

/// Kathamo-style "search command history" palette.
struct HistorySearchState {
    pane_id: usize,
    input: Entity<InputState>,
    matches: Vec<String>,
    selected: usize,
    _subscription: gpui::Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalContextMenu {
    pub pane_id: usize,
    pub position: Point<Pixels>,
}

struct RenamingProject {
    project_id: usize,
    input: Entity<InputState>,
    _subscription: gpui::Subscription,
}
/// Data read off the UI thread before restoring a workspace.
struct RestoreData {
    workspace: workspace_state::SavedWorkspace,
    scrollbacks: HashMap<String, String>,
    transcripts: HashMap<String, Vec<ChatMessage>>,
}

fn transcript_key(uid: &str, key: &str) -> String {
    format!("{uid}|{key}")
}

fn save_pane_tree(node: &PaneNode) -> workspace_state::SavedPaneNode {
    match node {
        PaneNode::Leaf(pane) => workspace_state::SavedPaneNode::Leaf { pane: *pane },
        PaneNode::Split {
            id,
            axis,
            left,
            right,
        } => workspace_state::SavedPaneNode::Split {
            id: *id,
            axis: match axis {
                gpui::Axis::Horizontal => workspace_state::SavedAxis::Horizontal,
                gpui::Axis::Vertical => workspace_state::SavedAxis::Vertical,
            },
            left: Box::new(save_pane_tree(left)),
            right: Box::new(save_pane_tree(right)),
        },
    }
}

fn restore_pane_tree(node: &workspace_state::SavedPaneNode) -> PaneNode {
    match node {
        workspace_state::SavedPaneNode::Leaf { pane } => PaneNode::Leaf(*pane),
        workspace_state::SavedPaneNode::Split {
            id,
            axis,
            left,
            right,
        } => PaneNode::Split {
            id: *id,
            axis: match axis {
                workspace_state::SavedAxis::Horizontal => gpui::Axis::Horizontal,
                workspace_state::SavedAxis::Vertical => gpui::Axis::Vertical,
            },
            left: Box::new(restore_pane_tree(left)),
            right: Box::new(restore_pane_tree(right)),
        },
    }
}

/// True when every leaf refers to a pane that actually spawned.
fn tree_matches_panes(node: &PaneNode, panes: &[TerminalPane]) -> bool {
    match node {
        PaneNode::Leaf(id) => panes.iter().any(|pane| pane.id == *id),
        PaneNode::Split { left, right, .. } => {
            tree_matches_panes(left, panes) && tree_matches_panes(right, panes)
        }
    }
}

impl AppState {
    fn saved_workspace(&self, cx: &App) -> workspace_state::SavedWorkspace {
        let projects = self
            .projects
            .iter()
            .map(|project| {
                let sessions = project
                    .sessions
                    .iter()
                    .map(|session| {
                        let panes = session
                            .panes
                            .iter()
                            .map(|pane| {
                                let live = pane
                                    .input_state
                                    .as_ref()
                                    .map(|input| input.read(cx).value().to_string())
                                    .or_else(|| pane.restore_draft.clone())
                                    .unwrap_or_default();
                                let (draft, agent_draft) = if pane.chat.active {
                                    (pane.chat.terminal_draft.clone(), live)
                                } else {
                                    (live, pane.chat.agent_draft.clone())
                                };
                                workspace_state::SavedPane {
                                    id: pane.id,
                                    uid: pane.uid.clone(),
                                    title: pane.title.clone(),
                                    display_title: pane.display_title.to_string(),
                                    cwd: pane.cwd.clone().or_else(|| self.pane_cwd_path(pane)),
                                    draft,
                                    agent_draft,
                                    command_history: pane
                                        .command_history
                                        .iter()
                                        .rev()
                                        .take(200)
                                        .rev()
                                        .cloned()
                                        .collect(),
                                    chat_active: pane.chat.active,
                                    chat: workspace_state::SavedChat {
                                        fast_mode: pane.chat.fast_mode,
                                        selected_model: pane.chat.selected_model.clone(),
                                        rpi_session_id: pane.chat.rpi_session_id.clone(),
                                        models: pane
                                            .chat
                                            .models
                                            .iter()
                                            .map(|(model, chat)| workspace_state::SavedModelChat {
                                                model: Some(model.clone()),
                                                rpi_session_id: chat.rpi_session_id.clone(),
                                                conversation: conversations::conversation_key(
                                                    Some(model),
                                                ),
                                            })
                                            .collect(),
                                    },
                                }
                            })
                            .collect();
                        workspace_state::SavedSession {
                            id: session.id,
                            uid: session.uid.clone(),
                            title: session.title.to_string(),
                            display_title: session.display_title.to_string(),
                            logo: session.logo.to_string(),
                            maximized_pane_id: session.maximized_pane_id,
                            focused_pane: self
                                .focused_pane_id
                                .filter(|id| session.panes.iter().any(|pane| pane.id == *id))
                                .or_else(|| session.panes.first().map(|pane| pane.id)),
                            tree: session
                                .pane_tree
                                .as_ref()
                                .map(save_pane_tree)
                                .unwrap_or_else(|| workspace_state::SavedPaneNode::Leaf {
                                    pane: session.panes.first().map(|pane| pane.id).unwrap_or(0),
                                }),
                            panes,
                        }
                    })
                    .collect();
                workspace_state::SavedProject {
                    id: project.id,
                    path: project.path.clone(),
                    name: project.name.to_string(),
                    expanded: project.expanded,
                    active_session: project.active_session_idx,
                    sessions,
                }
            })
            .collect();
        workspace_state::SavedWorkspace {
            version: workspace_state::CURRENT_VERSION,
            active_project: self.active_project().map(|project| project.path.clone()),
            projects,
        }
    }

    fn saved_settings(&self) -> settings::AppSettings {
        let mut saved = self.settings.clone();
        saved.sidebar.visible = self.sidebar_visible;
        saved.sidebar.width = f32::from(self.sidebar_width);
        saved.theme = self.theme_preference;
        saved.palette_scheme = self.palette_scheme;
        saved.default_agent_model = self.default_agent_model.clone();
        if let Some(window) = self.last_window_bounds {
            saved.window = Some(window);
        }
        saved
    }

    fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.schedule_workspace_save(cx);
    }

    fn schedule_workspace_save(&mut self, cx: &Context<Self>) {
        self.startup_loaded = true;
        self.dirty = true;
        if !self.persistence_enabled {
            return;
        }
        let Some(path) = self.persistence_path.clone() else {
            return;
        };
        let workspace = self.saved_workspace(cx);
        let settings = self.saved_settings();
        let generation = self.save_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation_counter = self.save_generation.clone();
        let executor = cx.background_executor().clone();
        executor
            .clone()
            .spawn(async move {
                executor.timer(std::time::Duration::from_millis(300)).await;
                match workspace_state::save_if_current(
                    &path,
                    &workspace,
                    &generation_counter,
                    generation,
                ) {
                    Ok(true) => {}
                    // A newer snapshot superseded this one; do not write the
                    // settings that belong to this stale generation.
                    Ok(false) => return,
                    Err(error) => eprintln!("failed to save workspace state: {error}"),
                }
                if let Err(error) = settings::save_atomic(&settings) {
                    eprintln!("failed to save settings: {error}");
                }
            })
            .detach();
    }

    fn capture_window_bounds(&mut self, cx: &mut App) {
        let handles = cx.windows();
        for handle in handles {
            if let Ok(bounds) = handle.update(cx, |_root, window, _cx| window.window_bounds()) {
                self.last_window_bounds = Some(settings::SavedWindow::from_bounds(bounds));
                break;
            }
        }
    }

    fn pane_scrollback(&self, pane: &TerminalPane, cx: &App) -> Option<String> {
        let terminal = pane.terminal.read(cx);
        if terminal.is_alt_screen() {
            return None;
        }
        let text = terminal.buffer_text(workspace_state::max_scrollback_lines());
        (!text.trim().is_empty()).then_some(text)
    }

    /// Synchronous, best-effort flush used when the window closes or the app
    /// quits. Writes settings, workspace, scrollback and conversation logs so a
    /// normal close never loses the last few seconds of work.
    fn save_all_sync(&mut self, window: Option<&Window>, cx: &mut App) {
        self.startup_loaded = true;
        if !self.persistence_enabled {
            self.dirty = false;
            return;
        }
        self.save_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(window) = window {
            self.last_window_bounds =
                Some(settings::SavedWindow::from_bounds(window.window_bounds()));
        } else {
            self.capture_window_bounds(cx);
        }

        let workspace = self.saved_workspace(cx);
        let settings = self.saved_settings();
        if let Some(path) = self.persistence_path.clone() {
            if let Err(error) = workspace_state::save_atomic(&path, &workspace) {
                eprintln!("failed to save workspace state: {error}");
            }
        }
        if let Err(error) = settings::save_atomic(&settings) {
            eprintln!("failed to save settings: {error}");
        }

        if self.settings.restore.scrollback {
            let mut written = HashSet::new();
            for project in &self.projects {
                for session in &project.sessions {
                    for pane in &session.panes {
                        if written.insert(pane.uid.clone()) {
                            if let Some(text) = self.pane_scrollback(pane, cx) {
                                if let Err(error) =
                                    workspace_state::write_scrollback(&pane.uid, &text)
                                {
                                    eprintln!("failed to save scrollback: {error}");
                                }
                            }
                        }
                    }
                }
            }
        }

        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if let Some(log) = pane.chat.conversation.as_mut() {
                        log.flush();
                    }
                    for chat in pane.chat.models.values_mut() {
                        if let Some(log) = chat.conversation.as_mut() {
                            log.flush();
                        }
                    }
                }
            }
        }
        self.dirty = false;
    }

    fn autosave(&mut self, cx: &mut Context<Self>) {
        if !self.dirty {
            return;
        }
        self.save_all_sync(None, cx);
    }

    fn apply_saved_workspace(&mut self, restored: RestoreData, cx: &mut Context<Self>) {
        if self.startup_loaded {
            return;
        }
        self.startup_loaded = true;

        self.sidebar_visible = self.settings.sidebar.visible;
        self.sidebar_width = px(self.settings.sidebar.width.max(120.0));
        self.theme_preference = self.settings.theme;
        self.palette_scheme = self.settings.palette_scheme;
        self.default_agent_model = self.settings.default_agent_model.clone();
        let mode = self.theme_preference.resolve_mode(cx.window_appearance());
        self.palette = load_palette(self.palette_scheme, mode.is_dark());
        gpui_component::Theme::change(mode, None, cx);
        sync_component_fonts(cx);

        let RestoreData {
            workspace,
            scrollbacks,
            transcripts,
        } = restored;
        let max_id = workspace.max_id();
        let active_project_path = workspace.active_project.clone();
        let mut focus_by_session: HashMap<usize, Option<usize>> = HashMap::new();

        self.projects.clear();
        self.active_project_idx = 0;

        for project in workspace.projects {
            if !project.path.is_dir() || self.projects.iter().any(|p| p.path == project.path) {
                continue;
            }
            let project_path = project.path.clone();
            let mut sessions = Vec::new();

            for saved_session in project.sessions {
                let mut panes = Vec::new();
                for saved_pane in saved_session.panes {
                    let dir = saved_pane
                        .cwd
                        .clone()
                        .filter(|cwd| cwd.is_dir())
                        .unwrap_or_else(|| project_path.clone());
                    let scrollback = if self.settings.restore.scrollback {
                        scrollbacks.get(&saved_pane.uid).cloned()
                    } else {
                        None
                    };
                    let weak = cx.entity().downgrade();
                    let Ok((terminal, pid, generation, terminal_subscription)) =
                        Self::spawn_terminal_view(
                            Some(&dir),
                            self.palette.clone(),
                            weak,
                            saved_pane.id.max(1),
                            scrollback.as_deref(),
                            cx,
                        )
                    else {
                        continue;
                    };

                    let title = SharedString::from(if saved_pane.title.is_empty() {
                        "terminal".to_string()
                    } else {
                        saved_pane.title.clone()
                    });
                    let display_title =
                        SharedString::from(if saved_pane.display_title.is_empty() {
                            title.to_string()
                        } else {
                            saved_pane.display_title.clone()
                        });
                    let mut pane = TerminalPane::new(
                        saved_pane.id,
                        generation,
                        terminal,
                        title,
                        display_title,
                        pid,
                    );
                    pane.uid = if saved_pane.uid.is_empty() {
                        paths::uid("p")
                    } else {
                        saved_pane.uid.clone()
                    };
                    pane.cwd = Some(dir);
                    pane.command_history = saved_pane.command_history;
                    pane.restore_draft = Some(saved_pane.draft.clone());
                    pane.chat.active = saved_pane.chat_active;
                    pane.chat.fast_mode = saved_pane.chat.fast_mode;
                    pane.chat.selected_model = saved_pane
                        .chat
                        .selected_model
                        .clone()
                        .or_else(|| self.effective_default_agent_model().cloned());
                    pane.chat.rpi_session_id = saved_pane.chat.rpi_session_id.clone();
                    pane.chat.terminal_draft = saved_pane.draft.clone();
                    pane.chat.agent_draft = saved_pane.agent_draft.clone();

                    if self.settings.restore.agent_history {
                        let default_key = conversations::conversation_key(None);
                        pane.chat.conversation =
                            Some(conversations::Log::open(&pane.uid, &default_key));
                        if let Some(messages) =
                            transcripts.get(&transcript_key(&pane.uid, &default_key))
                        {
                            pane.chat.messages = messages.clone();
                        }
                        for saved_model in &saved_pane.chat.models {
                            let Some(model) = saved_model.model.clone() else {
                                continue;
                            };
                            let key = conversations::conversation_key(Some(&model));
                            let mut chat = ModelChat {
                                rpi_session_id: saved_model.rpi_session_id.clone(),
                                conversation: Some(conversations::Log::open(&pane.uid, &key)),
                                ..Default::default()
                            };
                            if let Some(messages) =
                                transcripts.get(&transcript_key(&pane.uid, &key))
                            {
                                chat.messages = messages.clone();
                            }
                            pane.chat.models.insert(model, chat);
                        }
                    }

                    pane.terminal_subscription = Some(terminal_subscription);
                    panes.push(pane);
                }

                if panes.is_empty() {
                    continue;
                }
                let tree = restore_pane_tree(&saved_session.tree);
                let tree = if tree_matches_panes(&tree, &panes)
                    && panes.iter().all(|pane| tree.contains(pane.id))
                {
                    tree
                } else {
                    PaneNode::Leaf(panes[0].id)
                };
                let maximized = saved_session
                    .maximized_pane_id
                    .filter(|id| panes.iter().any(|pane| pane.id == *id));
                let title = SharedString::from(if saved_session.title.is_empty() {
                    "terminal".to_string()
                } else {
                    saved_session.title.clone()
                });
                let display_title = SharedString::from(if saved_session.display_title.is_empty() {
                    title.to_string()
                } else {
                    saved_session.display_title.clone()
                });
                let session_id = saved_session.id.max(1);
                focus_by_session.insert(session_id, saved_session.focused_pane);
                sessions.push(Session {
                    id: session_id,
                    uid: if saved_session.uid.is_empty() {
                        paths::uid("s")
                    } else {
                        saved_session.uid.clone()
                    },
                    title,
                    display_title,
                    logo: if saved_session.logo.is_empty() {
                        logos::default_logo()
                    } else {
                        SharedString::from(saved_session.logo)
                    },
                    pane_tree: Some(tree),
                    maximized_pane_id: maximized,
                    panes,
                    status: SessionStatus::Idle,
                });
            }

            if sessions.is_empty() {
                continue;
            }
            let mut active_session_idx = project.active_session.min(sessions.len() - 1);
            if active_session_idx >= sessions.len() {
                active_session_idx = 0;
            }
            self.projects.push(Project {
                id: if project.id == 0 {
                    let id = self.next_id;
                    self.next_id += 1;
                    id
                } else {
                    project.id
                },
                name: SharedString::from(project.name),
                path: project.path,
                sessions,
                active_session_idx,
                expanded: project.expanded,
            });
        }

        self.next_id = self.next_id.max(max_id + 1);
        self.active_project_idx = active_project_path
            .as_ref()
            .and_then(|path| {
                self.projects
                    .iter()
                    .position(|project| &project.path == path)
            })
            .unwrap_or(0);

        if self.projects.is_empty() {
            let default_dir = std::env::current_dir()
                .ok()
                .or_else(|| directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("/"));
            let name = default_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            let id = self.next_id;
            self.next_id += 1;
            let weak = cx.entity().downgrade();
            if let Ok((terminal, pid, generation, terminal_subscription)) =
                Self::spawn_terminal_view(
                    Some(&default_dir),
                    self.palette.clone(),
                    weak,
                    id,
                    None,
                    cx,
                )
            {
                let title = SharedString::from("terminal 1");
                let mut pane =
                    TerminalPane::new(id, generation, terminal, title.clone(), title.clone(), pid);
                pane.cwd = Some(default_dir.clone());
                pane.chat.selected_model = self.effective_default_agent_model().cloned();
                pane.terminal_subscription = Some(terminal_subscription);
                let session = Session {
                    id,
                    uid: paths::uid("s"),
                    title: title.clone(),
                    display_title: title,
                    logo: logos::default_logo(),
                    pane_tree: Some(PaneNode::Leaf(id)),
                    maximized_pane_id: None,
                    panes: vec![pane],
                    status: SessionStatus::Idle,
                };
                self.projects.push(Project {
                    id,
                    name: SharedString::from(name),
                    path: default_dir,
                    sessions: vec![session],
                    active_session_idx: 0,
                    expanded: true,
                });
                self.focused_pane_id = Some(id);
                self.active_project_idx = 0;
            }
        } else {
            let project = &self.projects[self.active_project_idx];
            let session =
                &project.sessions[project.active_session_idx.min(project.sessions.len() - 1)];
            self.focused_pane_id = focus_by_session
                .get(&session.id)
                .copied()
                .flatten()
                .filter(|id| session.panes.iter().any(|pane| pane.id == *id))
                .or_else(|| session.panes.first().map(|pane| pane.id));
        }

        self.rebuild_session_index();
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    pub fn tokens(&self, cx: &App) -> ThemeTokens {
        let mode = gpui_component::Theme::global(cx).mode;
        ThemeTokens::from_mode_with_scheme(mode, self.palette_scheme)
    }

    #[allow(dead_code)]
    pub fn tokens_with_appearance(&self, appearance: gpui::WindowAppearance) -> ThemeTokens {
        let mode = self.theme_preference.resolve_mode(appearance);
        ThemeTokens::from_mode_with_scheme(mode, self.palette_scheme)
    }

    /// Switch the terminal palette / block chrome scheme (Warp / GitHub).
    pub fn set_palette_scheme(
        &mut self,
        scheme: PaletteScheme,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.palette_scheme == scheme {
            return;
        }
        self.palette_scheme = scheme;
        self.apply_theme_preference(window, cx);
        self.schedule_workspace_save(cx);
    }

    pub fn set_theme_preference(
        &mut self,
        preference: ThemePreference,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.theme_preference == preference {
            return;
        }
        self.theme_preference = preference;
        self.apply_theme_preference(window, cx);
        self.schedule_workspace_save(cx);
    }

    fn observe_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.observe_window_appearance(window, |state, window, cx| {
            if state.theme_preference == ThemePreference::System {
                state.apply_theme_preference(Some(window), cx);
            }
        })
        .detach();
    }

    pub fn apply_theme_preference(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let appearance = window
            .as_ref()
            .map(|w| w.appearance())
            .unwrap_or_else(|| cx.window_appearance());
        let mode = self.theme_preference.resolve_mode(appearance);
        let is_dark = mode.is_dark();
        self.palette = load_palette(self.palette_scheme, is_dark);
        gpui_component::Theme::change(mode, window, cx);
        sync_component_fonts(cx);

        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    let mut config = pane.terminal.read(cx).config().clone();
                    config.colors = self.palette.clone();
                    pane.terminal.update(cx, |term, cx| {
                        term.update_config(config, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    fn make_drag_area(&self, cx: &Context<Self>) -> gpui::Div {
        div()
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, _| {
                    if ev.click_count == 2 {
                        this.drag_start_pos = None;
                        window.zoom_window();
                    } else {
                        this.drag_start_pos = Some(ev.position);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.drag_start_pos = None;
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, _| {
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.drag_start_pos = None;
                    return;
                }
                if let Some(start_pos) = this.drag_start_pos {
                    let dx = (ev.position.x - start_pos.x).to_f64();
                    let dy = (ev.position.y - start_pos.y).to_f64();
                    if dx * dx + dy * dy >= 64.0 {
                        this.drag_start_pos = None;
                        window.start_window_move();
                    }
                }
            }))
    }

    fn active_project(&self) -> Option<&Project> {
        self.projects.get(self.active_project_idx)
    }

    #[cfg(test)]
    fn active_project_mut(&mut self) -> Option<&mut Project> {
        self.projects.get_mut(self.active_project_idx)
    }

    fn active_session(&self) -> Option<&Session> {
        let project = self.active_project()?;
        project.sessions.get(project.active_session_idx)
    }

    /// Panes the person can actually see: the selected screen's panes, minus
    /// the siblings a maximized pane hides.
    fn visible_pane_ids(&self) -> HashSet<usize> {
        self.active_session()
            .map(|session| {
                let max = session.maximized_pane_id;
                session
                    .panes
                    .iter()
                    .filter(|pane| max.map_or(true, |m| m == pane.id))
                    .map(|pane| pane.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn spawn_terminal_view(
        dir: Option<&PathBuf>,
        palette: ColorPalette,
        window_title_target: gpui::WeakEntity<Self>,
        pane_id: usize,
        scrollback: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(
        Entity<TerminalView>,
        Option<u32>,
        String,
        gpui::Subscription,
    )> {
        let (shell_cmd, shell_name) = detect_shell();
        #[cfg(unix)]
        let generation = status_bus::new_generation();
        #[cfg(not(unix))]
        let generation = format!("pane-{pane_id}-{}", std::process::id());
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("PTY open error: {e}"))?;

        let mut cmd = CommandBuilder::new(&shell_cmd);
        if let Some(d) = dir {
            cmd.cwd(d);
        }
        shell_integration::configure_shell(&mut cmd, &shell_name);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "kathamo");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("LC_ALL", "en_US.UTF-8");
        if let Ok(executable) = std::env::current_exe() {
            cmd.env("KATHAMO_EXECUTABLE", &executable);
            cmd.env("MARKUP_EXECUTABLE", &executable);
        }
        #[cfg(unix)]
        if let Some(bus) = STATUS_BUS.as_ref() {
            kathamo_compat::register(&bus.app_id, pane_id, &generation);
            cmd.env("KATHAMO_STATUS_SOCKET", &bus.socket);
            cmd.env("KATHAMO_APP_ID", &bus.app_id);
            cmd.env("KATHAMO_PANE_ID", pane_id.to_string());
            cmd.env("KATHAMO_PANE_GENERATION", &generation);
            cmd.env("MARKUP_STATUS_SOCKET", &bus.socket);
            cmd.env("MARKUP_APP_ID", &bus.app_id);
            cmd.env("MARKUP_PANE_ID", pane_id.to_string());
            cmd.env("MARKUP_PANE_GENERATION", &generation);
            // Native Kathamo reporters (including Command Code) can use the
            // scoped reporting API. The opaque ID is bound to this generation.
            cmd.env("KATHAMO_ENV", "1");
            cmd.env("KATHAMO_SOCKET_PATH", &bus.socket);
            cmd.env("KATHAMO_PANE_ID", &generation);
            cmd.env("HERDR_ENV", "1");
            cmd.env("HERDR_SOCKET_PATH", &bus.socket);
            cmd.env("HERDR_PANE_ID", &generation);
            // A new pane is independent of a Command Code process that may
            // have launched Kathamo; its inherited child-owner guard must not
            // disable the vendor reporter for the new foreground agent.
            cmd.env_remove("CMD_KATHAMO_PANE_OWNER");
            cmd.env_remove("CMD_HERDR_PANE_OWNER");
            if let Ok(executable) = std::env::current_exe() {
                cmd.env("KATHAMO_BIN_PATH", &executable);
                cmd.env("HERDR_BIN_PATH", executable);
            }
            let snapshot_path = status_bus::snapshot_path(bus, pane_id, &generation);
            cmd.env("KATHAMO_STATUS_SNAPSHOT", &snapshot_path);
            cmd.env("MARKUP_STATUS_SNAPSHOT", &snapshot_path);
        }
        #[cfg(unix)]
        if let Some(tty) = pair.master.tty_name() {
            cmd.env("KATHAMO_STATUS_TTY", &tty);
            cmd.env("MARKUP_STATUS_TTY", &tty);
        }

        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            #[cfg(unix)]
            kathamo_compat::unregister(&generation);
            anyhow::anyhow!("Spawn shell error: {e}")
        })?;
        let pid = child.process_id();

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let pty_master = Arc::new(Mutex::new(pair.master));
        let read_only = Arc::new(AtomicBool::new(false));

        let pty_writer: Arc<Mutex<Box<dyn std::io::Write + Send>>> = Arc::new(Mutex::new(writer));
        let shared_writer = SharedWriter {
            writer: pty_writer,
            read_only,
        };

        let config = TerminalConfig {
            font_family: MONO_FONT.into(),
            font_size: px(13.5),
            cols: 80,
            rows: 24,
            scrollback: 10_000,
            line_height_multiplier: 1.2,
            padding: Edges::all(px(8.0)),
            colors: palette,
            shell_token: shell_integration::shell_token().to_string(),
            cursor_blink: true,
            copy_on_select: true,
            right_click_paste: false,
            alternate_scroll: true,
            detect_path_links: true,
            show_scrollbar: true,
            read_only: true,
            ..TerminalConfig::default()
        };

        let pty_for_resize = pty_master;
        let resize_callback = move |cols: usize, rows: usize| {
            let _ = pty_for_resize.lock().resize(PtySize {
                cols: cols as u16,
                rows: rows as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        };

        let target = window_title_target.clone();
        let seed = scrollback.map(str::to_string);
        let terminal = cx.new(|cx| {
            TerminalView::new_with_scrollback(shared_writer, reader, config, seed, cx)
                .with_resize_callback(resize_callback)
                .with_title_callback(move |window, cx, title| {
                    window.set_window_title(title);
                    let title_str = title.to_string();
                    let _ = target.update(cx, |state, cx| {
                        state.update_pane_title(pane_id, &title_str, cx);
                    });
                })
            // Exit status is intentionally not set here: the status poller
            // derives Done/Error from the real exit code (see poll_sessions_status).
        });

        // Shell-integration events arrive on the UI thread from the reader task.
        // The subscription is stored on the pane so it survives until the pane
        // is closed.
        let terminal_subscription = cx.subscribe(
            &terminal,
            move |state, _terminal, event: &ShellEvent, cx| {
                state.on_shell_event(pane_id, event, cx);
            },
        );

        Ok((terminal, pid, generation, terminal_subscription))
    }

    fn update_pane_title(&mut self, pane_id: usize, title: &str, cx: &mut Context<Self>) {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|p| p.sessions.get_mut(s_idx))
        else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        if pane.title == trimmed {
            return;
        }
        pane.title = trimmed.to_string();
        let next_display = truncate_title(&SharedString::from(trimmed.to_string()));
        pane.display_title = next_display.clone();
        // Mirror the pane the user is working in onto the session's sidebar
        // entry. Either the focused pane or the first pane of the screen
        // (when focus has not moved yet) is the primary.
        let is_primary = self.focused_pane_id == Some(pane_id)
            || session
                .pane_tree
                .as_ref()
                .is_some_and(|t| t.first_leaf() == pane_id);
        if is_primary {
            session.title = SharedString::from(trimmed.to_string());
            session.display_title = next_display.clone();
            let title_logo =
                logos::resolve_session_logo(&[], Some(trimmed), session.status, &session.logo);
            if title_logo != logos::default_logo() && session.logo != title_logo {
                session.logo = title_logo;
            }
        }
        self.mark_dirty(cx);
        cx.notify();
    }

    /// Periodic detection tick, Kathamo-style: screen scrape + OSC title +
    /// foreground process tree, with sticky Done/Error and focus resolution.
    /// Every pane of every session is polled; the session badge is the
    /// highest-ranked pane status and the sidebar title/logo follow the pane
    /// the user is working in.
    ///
    /// This variant gathers process information on the caller's thread. The app
    /// loop uses [`Self::poll_sessions_status_with`] so the blocking `/proc`
    /// reads run on the background executor; this entry point remains for tests
    /// and for callers that already have a snapshot.
    #[allow(dead_code)]
    pub fn poll_sessions_status(&mut self, cx: &mut Context<Self>) {
        let foregrounds = self.gather_foreground_processes();
        self.poll_sessions_status_with(&foregrounds, cx);
    }

    /// Shell pid of every live pane, deduplicated. Cheap and UI-thread safe:
    /// the background inspector needs a list, not an entity borrow.
    pub fn pane_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        for project in &self.projects {
            for session in &project.sessions {
                for pane in &session.panes {
                    if let Some(pid) = pane.pid
                        && !pids.contains(&pid)
                    {
                        pids.push(pid);
                    }
                }
            }
        }
        pids
    }

    #[cfg(unix)]
    fn apply_status_events(
        &mut self,
        events: Vec<status_bus::StatusEvent>,
        cx: &mut Context<Self>,
    ) {
        let Some(bus) = STATUS_BUS.as_ref() else {
            return;
        };
        let mut routed = Vec::new();
        for event in events {
            if event.app_id != bus.app_id {
                continue;
            }
            let Some(source) = self
                .projects
                .iter()
                .flat_map(|project| &project.sessions)
                .flat_map(|session| &session.panes)
                .find(|pane| {
                    pane.id as u64 == event.pane_id && pane.generation == event.pane_generation
                })
            else {
                continue;
            };
            if let Some(session_id) = event.session_id.as_ref() {
                for target in self
                    .projects
                    .iter()
                    .flat_map(|project| &project.sessions)
                    .flat_map(|session| &session.panes)
                    .filter(|pane| {
                        pane.id != source.id
                            && pane.native_session_id.as_ref() == Some(session_id)
                            && pane.native_session_reporter.as_deref()
                                == Some(event.reporter.as_str())
                    })
                {
                    let mut fanout = event.clone();
                    fanout.pane_id = target.id as u64;
                    fanout.pane_generation = target.generation.clone();
                    fanout.reporter_generation = format!("session-route:{session_id}");
                    fanout.sequence = FANOUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                    routed.push((fanout, true));
                }
            }
            routed.push((event, false));
        }
        for (event, is_routed) in routed {
            if event.app_id != bus.app_id {
                continue;
            }
            let Some(&(project_idx, session_idx, pane_idx)) =
                self.pane_index.get(&(event.pane_id as usize))
            else {
                continue;
            };
            let Some(pane) = self
                .projects
                .get_mut(project_idx)
                .and_then(|p| p.sessions.get_mut(session_idx))
                .and_then(|s| s.panes.get_mut(pane_idx))
            else {
                continue;
            };
            if pane.generation != event.pane_generation {
                continue;
            }
            if event.agent_id.is_some() {
                // Workers report to the parent pane; they never own its session.
            } else if event.event == "register" {
                if let Some(session_id) = event.session_id.as_ref() {
                    if pane
                        .native_session_id
                        .as_ref()
                        .is_some_and(|old| old != session_id)
                    {
                        pane.native_status = None;
                        pane.native_acknowledged = false;
                        pane.pending_approvals.clear();
                        pane.native_turn_id = None;
                        pane.ack_screen = None;
                        pane.poll_cache = None;
                        pane.status = SessionStatus::Unknown;
                    }
                    pane.native_session_id = Some(session_id.clone());
                    pane.native_session_reporter = Some(event.reporter.clone());
                }
            } else if let Some(session_id) = event.session_id.as_ref() {
                match (
                    pane.native_session_reporter.as_deref(),
                    pane.native_session_id.as_ref(),
                ) {
                    (Some(reporter), _) if reporter != event.reporter => continue,
                    (_, Some(current)) if current != session_id => continue,
                    _ => {
                        pane.native_session_id = Some(session_id.clone());
                        pane.native_session_reporter = Some(event.reporter.clone());
                    }
                }
            }

            let source = if let Some(agent_id) = event.agent_id.as_ref() {
                format!(
                    "{}:{}:{agent_id}",
                    event.reporter, event.reporter_generation
                )
            } else if is_routed {
                format!(
                    "route:{}:{}",
                    event.reporter,
                    event.session_id.as_deref().unwrap_or("")
                )
            } else {
                format!("{}:{}", event.reporter, event.reporter_generation)
            };
            if !is_routed {
                let active_generation = pane.native_reporter_generation.get(&event.reporter);
                if active_generation.is_some_and(|active| active != &event.reporter_generation) {
                    if event.event != "register" {
                        continue;
                    }
                    let prefix = format!("{}:", event.reporter);
                    pane.native_sequence
                        .retain(|key, _| !key.starts_with(&prefix));
                }
                pane.native_reporter_generation
                    .insert(event.reporter.clone(), event.reporter_generation.clone());
            }
            if pane
                .native_sequence
                .get(&source)
                .is_some_and(|previous| event.sequence <= *previous)
            {
                continue;
            }
            pane.native_sequence.insert(source, event.sequence);
            // A worker reports to its parent pane: update the indented entry and
            // never let it drive the pane's own status or an alert.
            if let Some(agent_id) = event.agent_id.clone() {
                let status = match event.event.as_str() {
                    "working" => Some(SessionStatus::Working),
                    "blocked" => Some(SessionStatus::Blocked),
                    "done" => Some(SessionStatus::Done),
                    "error" => Some(SessionStatus::Error),
                    "idle" | "register" => Some(SessionStatus::Idle),
                    "clear" | "cancelled" => None,
                    _ => {
                        pane.is_agent = true;
                        continue;
                    }
                };
                match status {
                    Some(status) => {
                        let since = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let entry = pane.sub_agents.entry(agent_id.clone()).or_insert(
                            attention::SubAgent {
                                id: agent_id.clone(),
                                title: event.message.clone(),
                                status: SessionStatus::Idle,
                                since,
                                seq: since,
                            },
                        );
                        entry.status = status;
                        entry.since = since;
                        entry.seq = since;
                        if entry.title.is_none() {
                            entry.title = event.message.clone();
                        }
                    }
                    None => {
                        pane.sub_agents.remove(&agent_id);
                    }
                }
                pane.is_agent = true;
                continue;
            }
            let reporter_name = event.reporter.clone();
            let event_name = event.event.clone();
            if pane.native_acknowledged && matches!(event.event.as_str(), "done" | "error") {
                continue;
            }
            if let Some(turn_id) = event.turn_id.as_ref() {
                if pane
                    .native_turn_id
                    .as_ref()
                    .is_some_and(|old| old != turn_id)
                    && matches!(event.event.as_str(), "working" | "blocked")
                {
                    pane.pending_approvals.clear();
                    pane.native_status = None;
                    pane.ack_screen = None;
                }
                pane.native_turn_id = Some(turn_id.clone());
            }
            let next = match event.event.as_str() {
                "register" | "heartbeat" => None,
                "idle" => Some(latch_completion(
                    pane.native_status.unwrap_or(pane.status),
                    SessionStatus::Idle,
                )),
                "working" => {
                    pane.native_acknowledged = false;
                    pane.pending_approvals.clear();
                    Some(SessionStatus::Working)
                }
                "blocked" => {
                    pane.native_acknowledged = false;
                    pane.pending_approvals
                        .insert(event.approval_id.unwrap_or_else(|| "__single__".into()));
                    Some(SessionStatus::Blocked)
                }
                "approval_resolved" => {
                    if let Some(id) = event.approval_id {
                        if !pane.pending_approvals.remove(&id) && pane.pending_approvals.len() == 1
                        {
                            pane.pending_approvals.clear();
                        }
                    } else if pane.pending_approvals.len() == 1 {
                        pane.pending_approvals.clear();
                    }
                    Some(if pane.pending_approvals.is_empty() {
                        SessionStatus::Working
                    } else {
                        SessionStatus::Blocked
                    })
                }
                "done" => {
                    pane.pending_approvals.clear();
                    Some(SessionStatus::Done)
                }
                "error" => {
                    pane.pending_approvals.clear();
                    Some(SessionStatus::Error)
                }
                "clear" | "cancelled" => {
                    pane.pending_approvals.clear();
                    pane.native_status = None;
                    pane.native_acknowledged = true;
                    pane.osc_report_active = false;
                    pane.terminal
                        .update(cx, |terminal, _| terminal.clear_agent_status_reports());
                    pane.status = SessionStatus::Unknown;
                    pane.ack_screen = None;
                    pane.poll_cache = None;
                    None
                }
                _ => None,
            };
            if let Some(next) = next {
                pane.native_status = Some(next);
                pane.status = next;
                pane.is_agent = true;
                if pane.app.is_none() && !reporter_name.is_empty() {
                    pane.app = Some(SharedString::from(reporter_name.clone()));
                }
                pane.detection = status::DetectionResult::new(
                    next,
                    status::EvidenceSource::AgentReport,
                    status::Confidence::High,
                );
                if matches!(next, SessionStatus::Working | SessionStatus::Blocked) {
                    pane.ack_screen = None;
                }
                pane.poll_cache = None;
            }
            self.integration_feedback =
                SharedString::from(format!("Live reporting from {reporter_name}: {event_name}"));
        }
        cx.notify();
    }

    /// Read `/proc` for every pane on the calling thread. The app loop uses
    /// [`Self::poll_sessions_status_with`] and gathers these on the background
    /// executor instead, so the blocking reads never sit in a frame.
    #[allow(dead_code)]
    fn gather_foreground_processes(&self) -> HashMap<u32, Arc<[String]>> {
        self.pane_pids()
            .into_iter()
            .map(|pid| (pid, status::foreground_process_names(Some(pid))))
            .collect()
    }

    /// Poll every pane against a foreground-process snapshot gathered off the
    /// UI thread. The snapshot is keyed by shell pid; a pane whose pid is
    /// missing simply sees an empty foreground group for this tick.
    pub fn poll_sessions_status_with(
        &mut self,
        foregrounds: &HashMap<u32, Arc<[String]>>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(unix)]
        self.apply_status_events(status_bus::drain(), cx);
        let diag_poll = diagnostics::poll_start();
        let mut changed = false;
        let mut panes_polled = 0u64;
        let focused_id = self.focused_pane_id;
        // `T3_STATUS_DEBUG=1` dumps every detection pass: the facts that went in
        // and the badge that came out. Without this the poller is invisible.
        let debug = status::debug_enabled();
        let trace = status::trace_enabled();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                let default_primary_id = session.pane_tree.as_ref().map(|t| t.first_leaf());
                let pane_count = session.panes.len();
                let mut primary_idx: Option<usize> = None;
                let mut primary_focused = false;
                let mut primary_fg: Arc<[String]> = status::empty_foreground_names();

                for (pane_idx, pane) in session.panes.iter_mut().enumerate() {
                    panes_polled += 1;
                    let focused = focused_id == Some(pane.id);
                    // Reuse the same immutable foreground snapshot for status,
                    // cache keys, and the session logo (one /proc probe per pane,
                    // gathered off the UI thread).
                    let foreground = pane
                        .pid
                        .and_then(|pid| foregrounds.get(&pid))
                        .cloned()
                        .unwrap_or_else(status::unknown_foreground_names);

                    // The pane the session badge/title follows: the focused
                    // pane, else the first pane of the screen.
                    let is_default_primary = Some(pane.id) == default_primary_id || pane_count == 1;
                    if focused || (is_default_primary && !primary_focused) {
                        primary_focused = focused;
                        primary_idx = Some(pane_idx);
                        primary_fg = Arc::clone(&foreground);
                    }

                    // Background panes never render, so their PTY events
                    // (title updates, child exit) must be drained here or the
                    // status would go stale while the user is in another tab.
                    if !focused {
                        pane.terminal.update(cx, |term, cx| {
                            term.drain_background_events(cx);
                        });
                    }

                    // Keep the pane header title in step with the OSC title
                    // even for background panes (the callback only fires where
                    // a window is attached).
                    let title_changed = {
                        let terminal = pane.terminal.read(cx);
                        let title = terminal.title().unwrap_or("");
                        let trimmed = title.trim();
                        if !trimmed.is_empty() && pane.title.as_str() != trimmed {
                            pane.title = trimmed.to_string();
                            pane.display_title =
                                truncate_title(&SharedString::from(trimmed.to_string()));
                            true
                        } else {
                            false
                        }
                    };
                    if title_changed {
                        changed = true;
                    }

                    // Idle fast path: the screen revision changes on PTY output
                    // and resize, while focus, exit state, and foreground-group
                    // changes are checked independently. Stable panes therefore
                    // skip row extraction, string allocation, and screen hashing.
                    // The foreground snapshot is compared by pointer: the /proc
                    // cache returns the same allocation while the group is
                    // unchanged, so this is a load and a compare.
                    let (screen_revision, exited, mut agent_report, osc_progress) = {
                        let terminal = pane.terminal.read(cx);
                        (
                            terminal.screen_revision(),
                            terminal.has_exited(),
                            terminal.agent_status_report(),
                            terminal.osc_progress().to_owned(),
                        )
                    };
                    let report_was_cleared = agent_report.is_none() && pane.osc_report_active;
                    pane.osc_report_active = agent_report.is_some();
                    if report_was_cleared {
                        pane.status = SessionStatus::Idle;
                        pane.detection = status::DetectionResult::new(
                            SessionStatus::Idle,
                            status::EvidenceSource::NoSignal,
                            status::Confidence::None,
                        );
                        pane.ack_screen = None;
                        pane.native_status = None;
                        pane.native_acknowledged = true;
                        pane.pending_approvals.clear();
                        pane.osc_report_active = false;
                    }
                    let foreground_changed =
                        pane.poll_cache
                            .as_ref()
                            .is_some_and(|(_, _, _, old_foreground)| {
                                !Arc::ptr_eq(old_foreground, &foreground)
                            });
                    let new_foreground_job =
                        pane.poll_cache.as_ref().is_some_and(|(_, _, _, old)| {
                            foreground_changed
                                && !foreground.is_empty()
                                && !Arc::ptr_eq(&foreground, &status::unknown_foreground_names())
                                && !Arc::ptr_eq(old, &status::unknown_foreground_names())
                        });
                    if foreground_changed && new_foreground_job && !foreground.is_empty() {
                        pane.native_acknowledged = false;
                        pane.ack_screen = None;
                        if matches!(pane.status, SessionStatus::Done | SessionStatus::Error) {
                            // A new command invalidates a completed job's badge.
                            pane.status = SessionStatus::Idle;
                            pane.detection = status::DetectionResult::new(
                                SessionStatus::Idle,
                                status::EvidenceSource::NoSignal,
                                status::Confidence::None,
                            );
                            pane.native_status = None;
                            pane.pending_approvals.clear();
                            if agent_report.as_ref().is_some_and(|report| {
                                matches!(
                                    report.state,
                                    terminal::AgentReportState::Done
                                        | terminal::AgentReportState::Error
                                        | terminal::AgentReportState::Idle
                                )
                            }) {
                                pane.terminal.update(cx, |terminal, _| {
                                    terminal.clear_agent_status_reports()
                                });
                                agent_report = None;
                            }
                            changed = true;
                        }
                    }
                    let unchanged = pane.poll_cache.as_ref().is_some_and(
                        |(revision, was_focused, was_exited, old_foreground)| {
                            *revision == screen_revision
                                && *was_focused == focused
                                && *was_exited == exited
                                && Arc::ptr_eq(old_foreground, &foreground)
                        },
                    );
                    // The grid only moved if the revision did; a focus or
                    // process-group change still needs a fresh verdict but can
                    // reuse the rows already in the scratch buffer.
                    let screen_unchanged = pane
                        .poll_cache
                        .as_ref()
                        .is_some_and(|(revision, ..)| *revision == screen_revision);
                    if unchanged {
                        match pane.status {
                            // A finished pane stays latched until new evidence.
                            SessionStatus::Done => continue,
                            // Working panes still need the detector's idle
                            // transition, but the grid did not move since the
                            // last scrape: reuse the cached rows instead of
                            // re-extracting and re-allocating the screen.
                            SessionStatus::Working | SessionStatus::Unknown => {}
                            _ => continue,
                        }
                    }

                    // Scrape into the pane's reused buffer only after the cheap
                    // revision/focus/process checks show that something changed.
                    // Ask for the whole visible screen: fresh shells can print
                    // their prompt on row 0, not just the last dozen rows.
                    if !screen_unchanged || pane.screen_scratch.is_empty() {
                        let diag_scrape = diagnostics::scrape_start();
                        pane.terminal
                            .read(cx)
                            .bottom_lines_into(usize::MAX, &mut pane.screen_scratch);
                        diagnostics::scrape_end(diag_scrape);
                    }

                    let screen_fp = {
                        let terminal = pane.terminal.read(cx);
                        screen_fingerprint(&pane.screen_scratch, terminal.title().unwrap_or(""))
                    };
                    let poll_key = (screen_revision, focused, exited, Arc::clone(&foreground));

                    let mut matched_agent_rule = false;
                    let mut matched_agent_idle = false;
                    let mut reported_agent_state = false;
                    let (next, ack) = {
                        let diag_detect = diagnostics::detect_start();
                        let terminal = pane.terminal.read(cx);
                        let bottom_lines = &pane.screen_scratch;
                        let title = terminal.title().unwrap_or("");
                        let screen_text = bottom_lines.join("\n");
                        let detector_state = pane.detector.update(
                            &screen_text,
                            terminal.has_exited(),
                            std::time::Instant::now(),
                        );
                        // while the screen is unchanged the sticky badge stays
                        // cleared; new output re-arms it.
                        let ack = pane.ack_screen;
                        let acknowledged = ack == Some(screen_fp) || pane.native_acknowledged;

                        // The agent rule match behind this tick's verdict, for the
                        // debug dump. The live path below captures it for free;
                        // the exited path only needs it for the dump itself.
                        let mut debug_match = None;

                        // Exit of the PTY child wins: the real exit code decides
                        // Done (0) vs Error (non-zero / signal), Kathamo-style.
                        let next = if terminal.has_exited() {
                            if debug {
                                debug_match = status::explain(&StatusInput {
                                    lines: bottom_lines,
                                    title: Some(title),
                                    foreground: &foreground,
                                    detector_state: Some(detector_state),
                                    osc_progress: &osc_progress,
                                });
                            }
                            if acknowledged {
                                SessionStatus::Idle
                            } else {
                                match terminal.exit_status() {
                                    Some(0) | None => SessionStatus::Done,
                                    Some(_) => SessionStatus::Error,
                                }
                            }
                        } else if let Some(native) = pane.native_status {
                            reported_agent_state = true;
                            native
                        } else if let Some(report) = &agent_report {
                            reported_agent_state = true;
                            match report.state {
                                terminal::AgentReportState::Idle => {
                                    if focused {
                                        SessionStatus::Active
                                    } else {
                                        SessionStatus::Idle
                                    }
                                }
                                terminal::AgentReportState::Working => SessionStatus::Working,
                                terminal::AgentReportState::Done => SessionStatus::Done,
                                terminal::AgentReportState::Blocked => SessionStatus::Blocked,
                                terminal::AgentReportState::Error => SessionStatus::Error,
                            }
                        } else {
                            let input = StatusInput {
                                lines: bottom_lines,
                                title: Some(title),
                                foreground: &foreground,
                                detector_state: Some(detector_state),
                                osc_progress: &osc_progress,
                            };
                            // The match is already computed by detection, so the
                            // debug dump can use it without a second evaluation.
                            let (detected, rule_match) = status::evaluate_screen_with_match(
                                &screen_text,
                                &input,
                                pane.status,
                                focused,
                            );
                            matched_agent_rule = rule_match.is_some();
                            matched_agent_idle = rule_match.as_ref().is_some_and(|matched| {
                                matched.state == agent_rules::RuleState::Idle
                                    && !matched.skip_state_update
                            });
                            if !debug {
                                debug_match = None;
                            } else {
                                debug_match = rule_match.clone();
                            }
                            if acknowledged
                                && matches!(detected, SessionStatus::Done | SessionStatus::Error)
                            {
                                if focused {
                                    SessionStatus::Active
                                } else {
                                    SessionStatus::Idle
                                }
                            } else {
                                detected
                            }
                        };

                        if debug {
                            let rule = debug_match.as_ref().map_or_else(
                                || "(no rule)".to_string(),
                                |m| {
                                    format!(
                                        "{}/{} v{} source={} state={} priority={} region={} \
                                         visible(working/blocker/idle)={}/{}/{} skip_update={}",
                                        m.pack,
                                        m.rule,
                                        m.pack_version.as_deref().unwrap_or("?"),
                                        m.source,
                                        m.state.label(),
                                        m.priority,
                                        m.region,
                                        m.visible_working,
                                        m.visible_blocker,
                                        m.visible_idle,
                                        m.skip_state_update,
                                    )
                                },
                            );
                            let evidence = if terminal.has_exited() {
                                "process-exit"
                            } else if reported_agent_state {
                                "agent-report"
                            } else if matched_agent_rule {
                                "agent-rule"
                            } else if matches!(
                                next,
                                SessionStatus::Blocked
                                    | SessionStatus::Error
                                    | SessionStatus::Working
                            ) {
                                "screen-heuristic"
                            } else if !foreground.is_empty() {
                                "foreground-process"
                            } else {
                                "no-signal"
                            };
                            let report_state = agent_report
                                .as_ref()
                                .map(|report| format!("{:?}", report.state));
                            eprintln!(
                                "[status] pane={} focused={focused} exited={} exit={:?} \
                                 pid={:?} fg={foreground:?} pack={:?} title={title:?} \
                                 prev={} next={} detector={detector_state:?} evidence={evidence} \
                                 report_state={report_state:?} rule={rule}",
                                pane.id,
                                terminal.has_exited(),
                                terminal.exit_status(),
                                pane.pid,
                                status::pack_for(&foreground),
                                pane.status.label(),
                                next.label(),
                            );
                            let screen: Vec<&str> = bottom_lines
                                .iter()
                                .map(String::as_str)
                                .filter(|line| !line.trim().is_empty())
                                .collect();
                            let from = screen.len().saturating_sub(DEBUG_SCREEN_LINES);
                            for line in &screen[from..] {
                                eprintln!("[status]   | {line}");
                            }
                        }

                        diagnostics::detect_end(diag_detect);
                        (next, ack)
                    };

                    // A done badge needs a recognized agent to have been working
                    // and then either exit or return to its explicit idle rule.
                    // Merely having a quiet foreground process is not completion.
                    let agent_now = foreground.iter().any(|name| status::is_agent_process(name));
                    let process_state_unavailable = foreground
                        .iter()
                        .any(|name| name == "<process-state-unavailable>");
                    let agent_exited =
                        pane.agent_active && !agent_now && !process_state_unavailable;
                    if !process_state_unavailable {
                        pane.agent_active = agent_now;
                    }

                    let turn_finished = !reported_agent_state
                        && pane.status == SessionStatus::Working
                        && (matched_agent_idle || agent_exited)
                        && matches!(next, SessionStatus::Idle | SessionStatus::Active)
                        && (agent_now || agent_exited);
                    let just_finished = turn_finished;
                    let next = if just_finished
                        && matches!(next, SessionStatus::Idle | SessionStatus::Active)
                        && ack != Some(screen_fp)
                    {
                        SessionStatus::Done
                    } else {
                        next
                    };
                    if matches!(next, SessionStatus::Working | SessionStatus::Blocked) {
                        pane.native_acknowledged = false;
                    }

                    // Keep the result visible across focus changes and trailing
                    // idle reports. A new working/blocked state, foreground job,
                    // or explicit clear starts a new lifecycle.
                    let next = latch_completion(pane.status, next);

                    pane.ack_screen = ack;
                    let evidence = if exited {
                        (
                            status::EvidenceSource::ProcessExit,
                            status::Confidence::High,
                        )
                    } else if reported_agent_state {
                        (
                            status::EvidenceSource::AgentReport,
                            status::Confidence::High,
                        )
                    } else if matched_agent_rule {
                        (status::EvidenceSource::AgentRule, status::Confidence::High)
                    } else if agent_exited {
                        (
                            status::EvidenceSource::AgentLifecycle,
                            status::Confidence::High,
                        )
                    } else if matches!(
                        next,
                        SessionStatus::Blocked | SessionStatus::Error | SessionStatus::Working
                    ) {
                        (
                            status::EvidenceSource::ScreenHeuristic,
                            status::Confidence::Low,
                        )
                    } else if next == SessionStatus::Unknown && !foreground.is_empty() {
                        (
                            status::EvidenceSource::ForegroundProcess,
                            status::Confidence::Low,
                        )
                    } else {
                        (status::EvidenceSource::NoSignal, status::Confidence::None)
                    };
                    let previous_status = pane.status;
                    pane.detection = status::DetectionResult::new(next, evidence.0, evidence.1);
                    if trace && previous_status != next {
                        eprintln!(
                            "[status-trace] pane={} {} -> {} evidence={:?} confidence={:?}",
                            pane.id,
                            previous_status.label(),
                            next.label(),
                            evidence.0,
                            evidence.1
                        );
                    }
                    if pane.status != next {
                        let prev_status = pane.status;
                        pane.status = next;
                        changed = true;

                        if prev_status != SessionStatus::Working && next == SessionStatus::Working {
                            pane.bottom_input_visible = false;
                            pane.completion_menu = None;
                            pane.terminal
                                .update(cx, |term, _| term.set_read_only(false));
                        } else if (prev_status == SessionStatus::Working
                            && next != SessionStatus::Working)
                            || (next == SessionStatus::Blocked
                                && prev_status != SessionStatus::Blocked)
                        {
                            pane.bottom_input_visible = true;
                        }
                    }

                    if let Some(app) = agent_report
                        .as_ref()
                        .and_then(|report| report.app.clone())
                        .filter(|app| !app.is_empty())
                    {
                        pane.app = Some(SharedString::from(app));
                    }
                    pane.is_agent |= reported_agent_state
                        || matched_agent_rule
                        || agent_now
                        || pane.app.is_some();

                    pane.poll_cache = Some(poll_key);
                }

                // Session badge: the highest-ranked pane status wins, so one
                // blocked pane inside a screen is enough to flag the tab.
                let mut aggregate = SessionStatus::Idle;
                for pane in &session.panes {
                    if pane.status.rank() > aggregate.rank() {
                        aggregate = pane.status;
                    }
                }
                if session.status != aggregate {
                    session.status = aggregate;
                    changed = true;
                }

                // Sidebar title/logo follow the pane the user is working in.
                if let Some(idx) = primary_idx {
                    let (primary_title, primary_status) = {
                        let pane = &session.panes[idx];
                        (pane.title.clone(), pane.status)
                    };
                    let next_logo = logos::resolve_session_logo(
                        &primary_fg,
                        Some(&primary_title),
                        primary_status,
                        &session.logo,
                    );
                    if session.logo != next_logo {
                        session.logo = next_logo;
                        changed = true;
                    }
                    if session.title.as_str() != primary_title {
                        let shared = SharedString::from(primary_title);
                        session.title = shared;
                        session.display_title = truncate_title(&session.title);
                        changed = true;
                    }
                }
            }
        }
        // Kathamo's attention pass. It finalizes every pane's badge: a finish the
        // person missed becomes Done, and looking at a Done pane clears it. It
        // runs for every pane every tick because visibility and window focus
        // change without any screen output.
        let visible_ids = self.visible_pane_ids();
        let window_active = self.window_active;
        let focused_id = self.focused_pane_id;
        let primed = self.status_primed;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut attention_seq = self.attention_seq;
        let mut alerts: Vec<attention::AppAlert> = Vec::new();
        let mut attention_changed = false;
        for project in &mut self.projects {
            let project_name = project.name.clone();
            for session in &mut project.sessions {
                let place = format!("{project_name} · {}", session.display_title);
                for pane in &mut session.panes {
                    let focused = focused_id == Some(pane.id);
                    let looking = visible_ids.contains(&pane.id) && window_active;
                    let prev = pane.attention.last;
                    let before = pane.status;
                    let resolved = attention::resolve(
                        &mut pane.attention,
                        pane.status,
                        pane.is_agent,
                        focused,
                        looking,
                        now_ms,
                        &mut attention_seq,
                    );
                    // Looking clears a finish for good: acknowledge it so a
                    // sticky native/OSC `done` cannot re-arm when you look away.
                    if looking
                        && before == SessionStatus::Done
                        && resolved.status != SessionStatus::Done
                    {
                        pane.native_status = None;
                        pane.native_acknowledged = true;
                        pane.ack_screen =
                            Some(screen_fingerprint(&pane.screen_scratch, &pane.title));
                        pane.osc_report_active = false;
                        pane.pending_approvals.clear();
                        pane.poll_cache = None;
                        pane.terminal
                            .update(cx, |terminal, _| terminal.clear_agent_status_reports());
                    }
                    if pane.status != resolved.status {
                        pane.status = resolved.status;
                        attention_changed = true;
                    }
                    if primed
                        && let Some(kind) = attention::alert_for(prev, resolved.status, looking)
                    {
                        alerts.push(attention::AppAlert {
                            kind,
                            pane_id: pane.id,
                            name: pane_agent_name(pane),
                            place: place.clone(),
                            looking,
                        });
                    }
                }
                // Fold the finalized pane badges back into the session rollup.
                let mut aggregate = SessionStatus::Idle;
                for pane in &session.panes {
                    if pane.status.rank() > aggregate.rank() {
                        aggregate = pane.status;
                    }
                }
                if session.status != aggregate {
                    session.status = aggregate;
                    attention_changed = true;
                }
            }
        }
        self.attention_seq = attention_seq;
        self.status_primed = true;
        if !alerts.is_empty() {
            self.pending_alerts.extend(alerts);
        }
        changed |= attention_changed;
        #[cfg(unix)]
        if let Some(bus) = STATUS_BUS.as_ref()
            && status_snapshot_due()
        {
            let paths: Vec<_> = self
                .projects
                .iter()
                .flat_map(|project| &project.sessions)
                .flat_map(|session| &session.panes)
                .map(|pane| {
                    let (evidence, confidence) =
                        match (pane.detection.evidence, pane.detection.confidence) {
                            (status::EvidenceSource::ProcessExit, status::Confidence::High) => {
                                ("process-exit", "high")
                            }
                            (status::EvidenceSource::AgentReport, status::Confidence::High) => {
                                ("agent-report", "high")
                            }
                            (status::EvidenceSource::AgentRule, status::Confidence::High) => {
                                ("agent-rule", "high")
                            }
                            (status::EvidenceSource::AgentLifecycle, status::Confidence::High) => {
                                ("agent-lifecycle", "high")
                            }
                            (status::EvidenceSource::TerminalProgress, _) => {
                                ("terminal-progress", "medium")
                            }
                            (status::EvidenceSource::ScreenHeuristic, _) => {
                                ("screen-heuristic", "low")
                            }
                            (status::EvidenceSource::ForegroundProcess, _) => {
                                ("foreground-process", "low")
                            }
                            _ => ("no-signal", "none"),
                        };
                    let snapshot = status_bus::StatusSnapshot {
                        version: 1,
                        app_id: bus.app_id.clone(),
                        pane_id: pane.id as u64,
                        pane_generation: pane.generation.clone(),
                        state: pane.status.label().into(),
                        evidence: evidence.into(),
                        confidence: confidence.into(),
                        reporter: pane.native_session_reporter.clone(),
                        updated_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                    };
                    (
                        status_bus::snapshot_path(bus, pane.id, &pane.generation),
                        snapshot,
                    )
                })
                .collect();
            cx.background_executor()
                .spawn(async move {
                    for (path, snapshot) in paths {
                        let Ok(bytes) = serde_json::to_vec(&snapshot) else {
                            continue;
                        };
                        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
                        if std::fs::write(&temp, bytes).is_ok() {
                            let _ = std::fs::rename(temp, path);
                        }
                    }
                })
                .detach();
        }
        diagnostics::panes_polled(panes_polled);
        diagnostics::poll_end(diag_poll);
        if changed {
            cx.notify();
        }
    }

    /// Make a project the one shown in the workspace and focus its current
    /// pane. Expanding it is part of activation: selecting a project should
    /// always reveal the sessions that belong to it.
    fn activate_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else {
            return;
        };
        self.active_project_idx = project_idx;
        project.expanded = true;
        let session_idx = project
            .active_session_idx
            .min(project.sessions.len().saturating_sub(1));
        project.active_session_idx = session_idx;
        let first_pane = project
            .sessions
            .get(session_idx)
            .and_then(|s| s.panes.first());
        if let Some(pane) = first_pane {
            pane.terminal
                .read(cx)
                .focus_handle()
                .clone()
                .focus(window, cx);
        }
        self.focused_pane_id = first_pane.map(|pane| pane.id);
        cx.notify();
        self.schedule_workspace_save(cx);
    }

    fn toggle_project_expanded(&mut self, project_idx: usize, cx: &mut Context<Self>) {
        if let Some(project) = self.projects.get_mut(project_idx) {
            project.expanded = !project.expanded;
            cx.notify();
            self.schedule_workspace_save(cx);
        }
    }

    /// Create a session inside one specific project, switching the workspace
    /// to that project so the new terminal is the one being worked in. This
    /// backs the "+" on every project header, not just the active project.
    fn create_new_session_in_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let self_weak = cx.entity().downgrade();
        let next_id = self.next_id;
        self.next_id += 1;

        let Some(dir) = self.projects.get(project_idx).map(|p| p.path.clone()) else {
            return;
        };
        if let Ok((terminal, pid, generation, terminal_subscription)) = Self::spawn_terminal_view(
            Some(&dir),
            self.palette.clone(),
            self_weak,
            next_id,
            None,
            cx,
        ) {
            let session_count = self
                .projects
                .get(project_idx)
                .map(|p| p.sessions.len())
                .unwrap_or(0);
            let title = SharedString::from(format!("terminal {}", session_count + 1));
            let display_title = truncate_title(&title);
            let mut pane = TerminalPane::new(
                next_id,
                generation,
                terminal.clone(),
                title.clone(),
                display_title.clone(),
                pid,
            );
            pane.chat.selected_model = self.effective_default_agent_model().cloned();
            pane.terminal_subscription = Some(terminal_subscription);
            let (input_state, input_subscription) = build_pane_input(next_id, window, cx);
            input_state.update(cx, |state, cx| state.focus(window, cx));
            pane.input_state = Some(input_state);
            pane.input_subscription = Some(input_subscription);
            let session = Session {
                id: next_id,
                uid: paths::uid("s"),
                title: title.clone(),
                display_title: truncate_title(&title),
                logo: logos::default_logo(),
                pane_tree: Some(PaneNode::Leaf(next_id)),
                maximized_pane_id: None,
                panes: vec![pane],
                status: SessionStatus::Idle,
            };

            if let Some(project) = self.projects.get_mut(project_idx) {
                project.sessions.push(session);
                project.active_session_idx = project.sessions.len() - 1;
                project.expanded = true;
            }
            self.active_project_idx = project_idx;
            self.focused_pane_id = Some(next_id);
            self.rebuild_session_index();
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    /// New session in whichever project is currently open in the workspace.
    #[allow(dead_code)]
    fn create_new_session_for_active_project(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project_idx = self.active_project_idx;
        if self.projects.get(project_idx).is_none() {
            // With no project open there is nothing to add a session to, so
            // the request turns into "open a project first".
            self.select_folder_and_add_project(window, cx);
            return;
        }
        self.create_new_session_in_project(project_idx, window, cx);
    }

    /// Close one session (all of its panes) without touching the other
    /// sessions and projects. The project stays open, with an empty state that
    /// offers to create a new session.
    fn close_session(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else {
            return;
        };
        if session_idx >= project.sessions.len() {
            return;
        }
        let removed = project.sessions.remove(session_idx);
        for pane in &removed.panes {
            status::forget_pid(pane.pid);
        }
        if project.active_session_idx > session_idx {
            project.active_session_idx -= 1;
        }
        if project.active_session_idx >= project.sessions.len() {
            project.active_session_idx = project.sessions.len().saturating_sub(1);
        }
        let is_active_project = project_idx == self.active_project_idx;
        let pane_id = project
            .sessions
            .get(project.active_session_idx)
            .and_then(|s| s.panes.first())
            .map(|pane| pane.id);
        if is_active_project {
            if let Some(pane_id) = pane_id {
                self.focus_pane(pane_id, window, cx);
            } else {
                self.focused_pane_id = None;
            }
        }
        // Drop the removed terminals after forgetting their pids, so their
        // PTY masters close and the shells go away with them.
        drop(removed);
        self.rebuild_session_index();
        cx.notify();
        self.schedule_workspace_save(cx);
    }

    /// Close a whole project. Its sessions and PTYs go away; the remaining
    /// projects (and their sessions) are left exactly as they were.
    fn close_project(&mut self, project_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if project_idx >= self.projects.len() {
            return;
        }
        let project_id = self.projects[project_idx].id;
        if self.project_menu == Some(project_id) {
            self.project_menu = None;
        }
        if self.renaming_project.as_ref().map(|r| r.project_id) == Some(project_id) {
            self.renaming_project = None;
        }
        let was_active = project_idx == self.active_project_idx;
        let removed = self.projects.remove(project_idx);
        for session in &removed.sessions {
            for pane in &session.panes {
                status::forget_pid(pane.pid);
            }
        }
        if self.active_project_idx > project_idx {
            self.active_project_idx -= 1;
        }
        if self.active_project_idx >= self.projects.len() {
            self.active_project_idx = self.projects.len().saturating_sub(1);
        }
        drop(removed);
        if was_active && !self.projects.is_empty() {
            let next_idx = self.active_project_idx;
            self.activate_project(next_idx, window, cx);
        } else if self.projects.is_empty() {
            self.focused_pane_id = None;
        }
        self.rebuild_session_index();
        cx.notify();
        self.schedule_workspace_save(cx);
    }

    fn start_renaming_project(
        &mut self,
        project_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_menu = None;
        if let Some(project) = self.projects.iter().find(|p| p.id == project_id) {
            let current_name = project.name.clone();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(current_name));
            input.update(cx, |state, cx| {
                state.focus(window, cx);
                state.select_all_text(cx);
            });
            let subscription = cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let new_name = input.read(cx).value();
                    this.finish_renaming_project(project_id, new_name, cx);
                }
            });
            self.renaming_project = Some(RenamingProject {
                project_id,
                input,
                _subscription: subscription,
            });
        }
        cx.notify();
    }

    fn finish_renaming_project(
        &mut self,
        project_id: usize,
        new_name: SharedString,
        cx: &mut Context<Self>,
    ) {
        let trimmed = new_name.trim();
        if !trimmed.is_empty() {
            if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
                project.name = SharedString::from(trimmed.to_string());
                self.schedule_workspace_save(cx);
            }
        }
        self.renaming_project = None;
        cx.notify();
    }

    fn cancel_renaming_project(&mut self, cx: &mut Context<Self>) {
        self.renaming_project = None;
        cx.notify();
    }

    /// Open a folder as a project. Opening a folder that is already in the
    /// sidebar focuses that project (and revives it if it has no sessions)
    /// rather than creating a duplicate.
    fn open_project_path(&mut self, folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(existing) = self.projects.iter().position(|p| p.path == folder) {
            if self.projects[existing].sessions.is_empty() {
                self.create_new_session_in_project(existing, window, cx);
            } else {
                self.activate_project(existing, window, cx);
            }
            return;
        }

        let name = folder
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());

        let self_weak = cx.entity().downgrade();
        let next_id = self.next_id;
        self.next_id += 1;

        if let Ok((terminal, pid, generation, terminal_subscription)) = Self::spawn_terminal_view(
            Some(&folder),
            self.palette.clone(),
            self_weak,
            next_id,
            None,
            cx,
        ) {
            let title = SharedString::from("terminal 1");
            let mut pane = TerminalPane::new(
                next_id,
                generation,
                terminal.clone(),
                title.clone(),
                title.clone(),
                pid,
            );
            pane.chat.selected_model = self.effective_default_agent_model().cloned();
            pane.terminal_subscription = Some(terminal_subscription);
            let (input_state, input_subscription) = build_pane_input(next_id, window, cx);
            input_state.update(cx, |state, cx| state.focus(window, cx));
            pane.input_state = Some(input_state);
            pane.input_subscription = Some(input_subscription);
            let session = Session {
                id: next_id,
                uid: paths::uid("s"),
                title: title.clone(),
                display_title: title,
                logo: logos::default_logo(),
                pane_tree: Some(PaneNode::Leaf(next_id)),
                maximized_pane_id: None,
                panes: vec![pane],
                status: SessionStatus::Idle,
            };
            let project = Project {
                id: next_id,
                name: SharedString::from(name),
                path: folder,
                sessions: vec![session],
                active_session_idx: 0,
                expanded: true,
            };

            self.projects.push(project);
            self.active_project_idx = self.projects.len() - 1;
            self.rebuild_session_index();
            self.focused_pane_id = Some(next_id);
            cx.notify();
            self.schedule_workspace_save(cx);
        }
    }

    fn select_folder_and_add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            self.open_project_path(folder, window, cx);
        }
    }

    fn rebuild_session_index(&mut self) {
        self.session_index.clear();
        self.pane_index.clear();
        for (p_idx, project) in self.projects.iter().enumerate() {
            for (s_idx, session) in project.sessions.iter().enumerate() {
                self.session_index.insert(session.id, (p_idx, s_idx));
                for (pane_idx, pane) in session.panes.iter().enumerate() {
                    self.pane_index.insert(pane.id, (p_idx, s_idx, pane_idx));
                }
            }
        }
    }

    /// Split a pane inside its session's own screen: the new terminal becomes
    /// a sibling leaf in that session's layout tree. Other sessions' screens
    /// are untouched.
    fn split_pane(
        &mut self,
        target_pane_id: usize,
        axis: gpui::Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let self_weak = cx.entity().downgrade();
        let split_id = self.next_id;
        self.next_id += 1;
        let next_id = self.next_id;
        self.next_id += 1;
        let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&target_pane_id) else {
            return;
        };

        let dir = self.projects.get(p_idx).map(|p| p.path.clone());
        let pane_count = self
            .projects
            .get(p_idx)
            .and_then(|p| p.sessions.get(s_idx))
            .map(|s| s.panes.len())
            .unwrap_or(0);
        if let Ok((terminal, pid, generation, terminal_subscription)) = Self::spawn_terminal_view(
            dir.as_ref(),
            self.palette.clone(),
            self_weak,
            next_id,
            None,
            cx,
        ) {
            let title = SharedString::from(format!("terminal {}", pane_count + 1));
            let display_title = truncate_title(&title);
            let mut pane = TerminalPane::new(
                next_id,
                generation,
                terminal.clone(),
                title.clone(),
                display_title,
                pid,
            );
            pane.chat.selected_model = self.effective_default_agent_model().cloned();
            pane.terminal_subscription = Some(terminal_subscription);
            let (input_state, input_subscription) = build_pane_input(next_id, window, cx);
            input_state.update(cx, |state, cx| state.focus(window, cx));
            pane.input_state = Some(input_state);
            pane.input_subscription = Some(input_subscription);

            if let Some(project) = self.projects.get_mut(p_idx) {
                if let Some(session) = project.sessions.get_mut(s_idx) {
                    session.panes.push(pane);
                    session.maximized_pane_id = None;
                    if let Some(tree) = &mut session.pane_tree {
                        tree.split(target_pane_id, next_id, split_id, axis);
                    } else {
                        session.pane_tree = Some(PaneNode::Leaf(next_id));
                    }
                }
            }
            self.focused_pane_id = Some(next_id);
            self.rebuild_session_index();
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    fn toggle_maximize_pane(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&pane_id) else {
            return;
        };

        if let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|p| p.sessions.get_mut(s_idx))
        {
            if session.maximized_pane_id == Some(pane_id) {
                session.maximized_pane_id = None;
            } else {
                session.maximized_pane_id = Some(pane_id);
            }
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    /// Close one pane inside its session's screen. Closing the last pane also
    /// removes the session (screen) itself; an emptied project spawns a fresh
    /// session, like before.
    fn close_pane(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&pane_id) else {
            return;
        };

        let mut session_removed = false;
        let mut needs_new_session = false;
        if let Some(project) = self.projects.get_mut(p_idx) {
            if let Some(session) = project.sessions.get_mut(s_idx) {
                if session.maximized_pane_id == Some(pane_id) {
                    session.maximized_pane_id = None;
                }
                if let Some(closed_pane) = session.panes.iter().find(|pane| pane.id == pane_id) {
                    status::forget_pid(closed_pane.pid);
                }
                session.pane_tree = session.pane_tree.take().and_then(|t| t.remove(pane_id));
                session.panes.retain(|p| p.id != pane_id);
                if session.pane_tree.is_none() || session.panes.is_empty() {
                    session_removed = true;
                }
            }
            if session_removed {
                project.sessions.retain(|s| !s.panes.is_empty());
                if project.sessions.is_empty() {
                    needs_new_session = true;
                } else {
                    project.active_session_idx =
                        project.active_session_idx.min(project.sessions.len() - 1);
                }
            } else if let Some(session) = project.sessions.get(s_idx) {
                // Focus the surviving first pane of this screen.
                let next_leaf = session
                    .pane_tree
                    .as_ref()
                    .map(|t| t.first_leaf())
                    .and_then(|id| session.panes.iter().find(|p| p.id == id));
                if let Some(pane) = next_leaf {
                    let pid = pane.id;
                    self.focus_pane(pid, window, cx);
                }
            }
        }

        self.rebuild_session_index();

        if needs_new_session {
            self.create_new_session_in_project(p_idx, window, cx);
        } else {
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    fn focus_pane(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active_view = ActiveView::Workspace;
        self.focused_pane_id = Some(pane_id);
        if let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&pane_id) {
            self.active_project_idx = p_idx;
            if let Some(p) = self.projects.get_mut(p_idx) {
                p.active_session_idx = s_idx;
                if let Some(session) = p.sessions.get_mut(s_idx) {
                    if let Some(pane) = session.panes.iter_mut().find(|p| p.id == pane_id) {
                        if pane.chat.auth.is_some() {
                            return;
                        }
                        let should_show = pane.should_show_bottom_input(cx);
                        pane.terminal.update(cx, |term, _| {
                            term.set_read_only(should_show);
                        });
                        if should_show {
                            if let Some(input_state) = &pane.input_state {
                                input_state.update(cx, |state, cx| {
                                    state.focus(window, cx);
                                });
                            } else {
                                pane.terminal
                                    .read(cx)
                                    .focus_handle()
                                    .clone()
                                    .focus(window, cx);
                            }
                        } else {
                            pane.terminal
                                .read(cx)
                                .focus_handle()
                                .clone()
                                .focus(window, cx);
                        }
                    }
                }
            }
        }
        self.dirty = true;
        cx.notify();
    }

    fn focus_pane_input(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_ref(pane_id).is_some_and(|p| p.chat.auth.is_some()) {
            return;
        }
        self.active_view = ActiveView::Workspace;
        self.focused_pane_id = Some(pane_id);
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) {
            self.active_project_idx = p_idx;
            if let Some(pane) = self
                .projects
                .get_mut(p_idx)
                .and_then(|p| p.sessions.get_mut(s_idx))
                .and_then(|s| s.panes.get_mut(pane_idx))
            {
                if let Some(input_state) = &pane.input_state {
                    input_state.update(cx, |state, cx| {
                        state.focus(window, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    /// The model new and unpinned panes should use: the user's saved default,
    /// otherwise the default OMP itself is configured with.
    fn effective_default_agent_model(&self) -> Option<&agent_auth::AgentModel> {
        self.default_agent_model
            .as_ref()
            .or(self.fallback_agent_model.as_ref())
    }

    fn pane_ref(&self, pane_id: usize) -> Option<&TerminalPane> {
        let &(p_idx, s_idx, pane_idx) = self.pane_index.get(&pane_id)?;
        self.projects
            .get(p_idx)?
            .sessions
            .get(s_idx)?
            .panes
            .get(pane_idx)
    }

    fn pane_mut(&mut self, pane_id: usize) -> Option<&mut TerminalPane> {
        let &(p_idx, s_idx, pane_idx) = self.pane_index.get(&pane_id)?;
        self.projects
            .get_mut(p_idx)?
            .sessions
            .get_mut(s_idx)?
            .panes
            .get_mut(pane_idx)
    }

    /// Shell history plus in-session commands, oldest first.
    fn pane_history_entries(&self, pane: &TerminalPane) -> Vec<String> {
        shell_history::merge(&pane.shell_history, &pane.command_history)
    }

    /// Snapshot of everything suggestion code needs from a pane's input.
    fn input_context(
        &self,
        pane_id: usize,
        cx: &App,
    ) -> Option<(String, usize, PathBuf, Vec<String>, Entity<InputState>)> {
        let pane = self.pane_ref(pane_id)?;
        let input = pane.input_state.clone()?;
        let value = input.read(cx).value().to_string();
        let cursor = input.read(cx).cursor();
        let cwd = self
            .pane_cwd_path(pane)
            .unwrap_or_else(|| PathBuf::from("."));
        Some((value, cursor, cwd, self.pane_history_entries(pane), input))
    }

    /// Create a pane's input editor and subscribe to its changes.
    fn create_pane_input(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let (input, subscription) = build_pane_input(pane_id, window, cx);
        let restore = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.restore_draft.take());
        if let Some(text) = restore.filter(|text| !text.is_empty()) {
            set_input_text(&input, text, window, cx);
        }
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.input_state = Some(input.clone());
            pane.input_subscription = Some(subscription);
        }
        input
    }

    /// Keep ghost text and the completion menu in sync with typed text.
    fn on_pane_input_changed(
        &mut self,
        pane_id: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            if let Some(input) = self
                .pane_ref(pane_id)
                .and_then(|pane| pane.input_state.clone())
            {
                input.update(cx, |state, cx| state.clear_inline_completion(cx));
            }
            return;
        }
        let Some((value, cursor, cwd, entries, input)) = self.input_context(pane_id, cx) else {
            return;
        };
        let menu_open = self
            .pane_ref(pane_id)
            .is_some_and(|pane| pane.completion_menu.is_some());

        if menu_open {
            // Typing filters an open menu instead of dismissing it.
            self.refresh_completion_menu(pane_id, &value, cursor, &cwd, &entries);
            input.update(cx, |state, cx| state.clear_inline_completion(cx));
            return;
        }

        let suggestion = completion::history_suggestion(&value, cursor, &entries);
        input.update(cx, |state, cx| match suggestion {
            Some(text) if !text.is_empty() => state.set_inline_completion_text(text, cx),
            _ => state.clear_inline_completion(cx),
        });
    }

    /// Recompute the open completion menu for the current input value.
    fn refresh_completion_menu(
        &mut self,
        pane_id: usize,
        value: &str,
        cursor_utf16: usize,
        cwd: &Path,
        entries: &[String],
    ) {
        let cursor = completion::utf16_to_byte(value, cursor_utf16);
        let result = completion::complete(value, cursor, cwd, entries);
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        match result {
            Some(result) => {
                let token = value
                    .get(result.start..result.end)
                    .unwrap_or_default()
                    .to_string();
                let previous = pane
                    .completion_menu
                    .as_ref()
                    .filter(|menu| menu.token == token)
                    .map(|menu| menu.selected)
                    .unwrap_or(0);
                pane.completion_menu = Some(CompletionMenuState {
                    start: result.start,
                    end: result.end,
                    token,
                    selected: previous.min(result.items.len().saturating_sub(1)),
                    items: result.items,
                });
            }
            None => pane.completion_menu = None,
        }
    }

    fn close_completion_menu(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.completion_menu = None;
        }
        cx.notify();
    }

    fn move_completion_selection(&mut self, pane_id: usize, delta: isize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            if let Some(menu) = pane.completion_menu.as_mut() {
                if !menu.items.is_empty() {
                    let len = menu.items.len() as isize;
                    menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
                }
            }
        }
        cx.notify();
    }

    /// Tab: accept the only candidate, or open the menu when there are several.
    fn start_completion(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((value, cursor, cwd, entries, input)) = self.input_context(pane_id, cx) else {
            return;
        };

        // A visible ghost suggestion is the best candidate; Tab accepts it.
        let ghost = input.read(cx).inline_completion_text().map(str::to_string);
        if ghost.is_some() {
            input.update(cx, |state, cx| {
                state.accept_inline_completion(window, cx);
            });
            cx.notify();
            return;
        }

        // Even before the deferred change handler has painted ghost text, Tab
        // completes from history.
        if let Some(suffix) = completion::history_suggestion(&value, cursor, &entries) {
            let cursor = cursor + completion::utf16_len(&suffix);
            set_input_text_with_cursor(&input, format!("{value}{suffix}"), cursor, window, cx);
            cx.notify();
            return;
        }

        let cursor_byte = completion::utf16_to_byte(&value, cursor);
        let Some(result) = completion::complete(&value, cursor_byte, &cwd, &entries) else {
            self.close_completion_menu(pane_id, cx);
            return;
        };

        if result.items.len() == 1 {
            let item = result.items[0].clone();
            self.apply_completion(pane_id, result.start, result.end, item, window, cx);
        } else {
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.completion_menu = Some(CompletionMenuState {
                    start: result.start,
                    end: result.end,
                    token: value
                        .get(result.start..result.end)
                        .unwrap_or_default()
                        .to_string(),
                    items: result.items,
                    selected: 0,
                });
            }
            cx.notify();
        }
    }

    fn accept_completion_at(
        &mut self,
        pane_id: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.pane_ref(pane_id).and_then(|pane| {
            let menu = pane.completion_menu.as_ref()?;
            let item = menu.items.get(index)?.clone();
            Some((menu.start, menu.end, item))
        });
        let Some((start, end, item)) = selected else {
            return;
        };
        self.apply_completion(pane_id, start, end, item, window, cx);
    }

    /// Replace the completing token with `item` and keep the editor usable.
    fn apply_completion(
        &mut self,
        pane_id: usize,
        start: usize,
        end: usize,
        item: completion::CompletionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((value, _, _, _entries, input)) = self.input_context(pane_id, cx) else {
            return;
        };
        let start = start.min(value.len());
        let end = end.min(value.len());
        if !value.is_char_boundary(start) || !value.is_char_boundary(end) || start > end {
            return;
        }

        let mut new_value = String::with_capacity(value.len() + item.value.len() + 1);
        new_value.push_str(&value[..start]);
        new_value.push_str(&item.value);
        let mut cursor = start + item.value.len();
        new_value.push_str(&value[end..]);

        // Command and branch completions read better with the cursor moved to
        // the next token; file completions stay tight to the path.
        if !item.is_dir && item.kind != completion::CompletionKind::File {
            new_value.insert(cursor, ' ');
            cursor += 1;
        }

        let cursor_utf16 = completion::byte_to_utf16(&new_value, cursor);
        input.update(cx, |state, cx| {
            state.set_value(new_value, window, cx);
            let position = state.text().offset_to_position(
                cursor_utf16.min(completion::utf16_len(state.value().as_str())),
            );
            state.set_cursor_position(position, window, cx);
            state.clear_inline_completion(cx);
        });

        if item.is_dir {
            // Descend into the directory: keep completing the new token.
            if let Some((value, cursor, cwd, entries, _)) = self.input_context(pane_id, cx) {
                if let Some(pane) = self.pane_mut(pane_id) {
                    pane.completion_menu = None;
                }
                self.refresh_completion_menu(pane_id, &value, cursor, &cwd, &entries);
                if let Some(pane) = self.pane_mut(pane_id) {
                    if let Some(menu) = pane.completion_menu.as_mut() {
                        menu.selected = 0;
                    }
                }
            }
        } else if let Some(pane) = self.pane_mut(pane_id) {
            pane.completion_menu = None;
        }

        if let Some(pane) = self.pane_ref(pane_id) {
            if let Some(input_state) = pane.input_state.clone() {
                input_state.update(cx, |state, cx| state.focus(window, cx));
            }
        }
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // History search (Ctrl+R)
    // ---------------------------------------------------------------------

    fn open_history_search(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_search.is_some() {
            return;
        }
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let entries = self.pane_history_entries(pane);
        let initial = pane
            .input_state
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search command history")
                .default_value(initial.clone())
        });
        input.update(cx, |state, cx| {
            let position = state
                .text()
                .offset_to_position(completion::utf16_len(&initial));
            state.set_cursor_position(position, window, cx);
            state.focus(window, cx);
        });

        let matches: Vec<String> = shell_history::search(&entries, &initial)
            .into_iter()
            .cloned()
            .collect();

        let subscription = cx.subscribe_in(
            &input,
            window,
            move |_this: &mut Self, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let weak = cx.entity().downgrade();
                    window.defer(cx, move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.refresh_history_search(window, cx);
                        });
                    });
                }
            },
        );

        self.history_search = Some(HistorySearchState {
            pane_id,
            input,
            matches,
            selected: 0,
            _subscription: subscription,
        });
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.completion_menu = None;
        }
        cx.notify();
    }

    fn refresh_history_search(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let query = search.input.read(cx).value().to_string();
        let pane_id = search.pane_id;
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let entries = self.pane_history_entries(pane);
        let matches: Vec<String> = shell_history::search(&entries, &query)
            .into_iter()
            .cloned()
            .collect();
        if let Some(search) = self.history_search.as_mut() {
            search.matches = matches;
            search.selected = 0;
        }
        cx.notify();
    }

    fn move_history_search_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(search) = self.history_search.as_mut() {
            if !search.matches.is_empty() {
                let len = search.matches.len() as isize;
                search.selected = (search.selected as isize + delta).rem_euclid(len) as usize;
            }
        }
        cx.notify();
    }

    fn close_history_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane_id = self.history_search.as_ref().map(|search| search.pane_id);
        self.history_search = None;
        if let Some(pane_id) = pane_id {
            if let Some(input) = self
                .pane_ref(pane_id)
                .and_then(|pane| pane.input_state.clone())
            {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
        }
        cx.notify();
    }

    fn accept_history_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let Some(command) = search.matches.get(search.selected).cloned() else {
            let pane_id = search.pane_id;
            self.history_search = None;
            if let Some(input) = self
                .pane_ref(pane_id)
                .and_then(|pane| pane.input_state.clone())
            {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
            cx.notify();
            return;
        };
        let pane_id = search.pane_id;
        let input = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.input_state.clone());
        self.history_search = None;
        if let Some(input) = input {
            set_input_text(&input, command, window, cx);
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.history_index = None;
        }
        cx.notify();
    }

    fn accept_history_search_index(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(search) = self.history_search.as_mut() {
            search.selected = index;
        }
        self.accept_history_search(window, cx);
    }

    #[allow(dead_code)]
    fn clear_pane_input(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        if let Some(input_state) = &pane.input_state {
            input_state.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
        }
        pane.history_index = None;
        pane.pending_draft.clear();
        pane.completion_menu = None;
        cx.notify();
    }

    #[allow(dead_code)]
    fn cancel_pane_job(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        pane.terminal.update(cx, |term, cx| {
            term.send_bytes(b"\x03", cx);
        });
        cx.notify();
    }

    fn step_pane_history(
        &mut self,
        pane_id: usize,
        is_up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let Some(input_state) = pane.input_state.clone() else {
            return;
        };
        let entries = shell_history::merge(&pane.shell_history, &pane.command_history);
        if entries.is_empty() {
            return;
        }

        if is_up {
            let new_idx = match pane.history_index {
                None => {
                    pane.pending_draft = input_state.read(cx).value().to_string();
                    entries.len() - 1
                }
                Some(idx) => idx.saturating_sub(1),
            };
            pane.history_index = Some(new_idx);
            if let Some(cmd) = entries.get(new_idx).cloned() {
                set_input_text(&input_state, cmd, window, cx);
            }
        } else if let Some(idx) = pane.history_index {
            if idx + 1 < entries.len() {
                let new_idx = idx + 1;
                pane.history_index = Some(new_idx);
                if let Some(cmd) = entries.get(new_idx).cloned() {
                    set_input_text(&input_state, cmd, window, cx);
                }
            } else {
                pane.history_index = None;
                let draft = std::mem::take(&mut pane.pending_draft);
                set_input_text(&input_state, draft, window, cx);
            }
        }
        cx.notify();
    }
    fn toggle_chat_tool_expanded(&mut self, pane_id: usize, index: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            let messages = pane.chat.visible_messages_mut();
            if let Some(msg) = messages.get_mut(index) {
                if let Some(tool) = &mut msg.tool {
                    tool.expanded = !tool.expanded;
                    cx.notify();
                } else if msg.role == ChatRole::Thinking {
                    msg.expanded = !msg.expanded;
                    cx.notify();
                }
            }
        }
    }

    fn warm_agent_bridge(&mut self, pane_id: usize) {
        let save_history = self.settings.restore.agent_history;
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let cwd = self
            .pane_cwd_path(pane)
            .unwrap_or_else(|| PathBuf::from("."));
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if let Some(model) = pane.chat.selected_model.clone() {
            let fast = pane.chat.fast_mode;
            let pane_uid = pane.uid.clone();
            let model_key = conversations::conversation_key(Some(&model));
            let chat = pane.chat.models.entry(model.clone()).or_default();
            if save_history && chat.conversation.is_none() {
                chat.conversation = Some(conversations::Log::open(&pane_uid, &model_key));
            }
            if chat.bridge.is_none() {
                let bridge = match model.backend {
                    agent_auth::ModelBackend::Rpi => {
                        let selector = format!("{}/{}", model.provider, model.id);
                        agent_bridge::AgentBridge::spawn_resumed(
                            &cwd,
                            chat.rpi_session_id.as_deref(),
                            Some(&selector),
                        )
                        .or_else(|_| agent_bridge::AgentBridge::spawn_model(&cwd, Some(&selector)))
                        .map(ModelBridge::Rpi)
                    }
                    agent_auth::ModelBackend::Omp => {
                        let selector = format!("{}/{}", model.provider, model.id);
                        omp_bridge::OmpBridge::spawn(&cwd, &selector).map(ModelBridge::Omp)
                    }
                };
                match bridge {
                    Ok(mut bridge) => {
                        if !fast {
                            let _ = bridge.set_thinking_level("high");
                        }
                        chat.startup_error = None;
                        chat.bridge = Some(bridge);
                    }
                    Err(error) => chat.startup_error = Some(error),
                }
            }
        } else if save_history && pane.chat.conversation.is_none() {
            pane.chat.conversation = Some(conversations::Log::open(
                &pane.uid,
                &conversations::conversation_key(None),
            ));
        }
    }

    fn toggle_fast_mode(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        pane.chat.fast_mode = !pane.chat.fast_mode;
        let fast = pane.chat.fast_mode;
        let level = if fast { "low" } else { "high" };
        if let Some(model) = &pane.chat.selected_model {
            if let Some(chat) = pane.chat.models.get_mut(model) {
                if let Some(bridge) = chat.bridge.as_mut() {
                    let _ = bridge.set_thinking_level(level);
                }
            }
        }
        cx.notify();
    }

    fn set_chat_mode(
        &mut self,
        pane_id: usize,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if pane.chat.active == active {
            return;
        }
        let Some(input) = pane.input_state.clone() else {
            return;
        };
        let current = input.read(cx).value().to_string();
        let draft = if active {
            pane.chat.terminal_draft = current;
            std::mem::take(&mut pane.chat.agent_draft)
        } else {
            pane.chat.agent_draft = current;
            std::mem::take(&mut pane.chat.terminal_draft)
        };
        pane.chat.active = active;
        pane.completion_menu = None;
        pane.history_index = None;
        pane.pending_draft.clear();
        set_input_text(&input, draft, window, cx);
        self.focus_pane(pane_id, window, cx);
        if active {
            self.warm_agent_bridge(pane_id);
        }
    }
    fn open_agent_auth(
        &mut self,
        pane_id: usize,
        page: AgentDialogPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let cwd = self
            .pane_cwd_path(pane)
            .unwrap_or_else(|| PathBuf::from("."));
        self.focused_pane_id = Some(pane_id);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Enter API key or credential")
        });
        let provider_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Other provider ID"));
        let focus = cx.focus_handle();
        if page == AgentDialogPage::Auth {
            provider_input.update(cx, |state, cx| state.focus(window, cx));
        } else {
            focus.focus(window, cx);
        }
        let mut auth = AuthModal {
            page,
            focus,
            cwd,
            providers: Vec::new(),
            discovery: None,
            models: Vec::new(),
            model_discovery: None,
            model_error: None,
            provider_input,
            input,
            login: None,
            selected: None,
            output: String::new(),
            error: None,
        };
        auth.discover();
        if page == AgentDialogPage::Auth {
            if let Some(first) = auth.providers.first() {
                let id = first.id.clone();
                let spec = crate::provider::get_provider(&id);
                let primary_var = spec.and_then(|s| s.primary_env_var()).unwrap_or("API Key");
                let input_clone = auth.input.clone();
                auth.input.update(cx, |state, cx| {
                    state.set_placeholder(format!("Paste {primary_var} here..."), window, cx);
                });
                auth.selected = Some(id);
                window.defer(cx, move |window, cx| {
                    input_clone.update(cx, |state, cx| {
                        state.focus(window, cx);
                    });
                });
            }
        }
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.chat.auth = Some(auth);
            cx.notify();
        }
    }

    fn close_agent_auth(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            if let Some(mut auth) = pane.chat.auth.take() {
                if let Some(mut login) = auth.login.take() {
                    login.cancel();
                }
                auth.input
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
        }
        self.focus_pane(pane_id, window, cx);
        cx.notify();
    }

    fn set_agent_dialog_page(
        &mut self,
        pane_id: usize,
        page: AgentDialogPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(auth) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.chat.auth.as_mut())
        else {
            return;
        };
        if page == AgentDialogPage::Models && auth.page != page {
            if let Some(mut login) = auth.login.take() {
                login.cancel();
            }
            auth.input
                .update(cx, |state, cx| state.set_value("", window, cx));
            auth.selected = None;
            auth.output.clear();
            auth.error = None;
        }
        auth.page = page;
        if page == AgentDialogPage::Models {
            auth.focus.focus(window, cx);
        } else {
            auth.provider_input
                .update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn select_agent_model(
        &mut self,
        pane_id: usize,
        model: Option<agent_auth::AgentModel>,
        make_default: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.chat.startup_error = None;
            if let Some(model) = model.as_ref() {
                pane.chat
                    .models
                    .entry(model.clone())
                    .or_default()
                    .startup_error = None;
            }
            pane.chat.selected_model = model.clone();
        }
        if make_default {
            if model.is_some() {
                self.default_agent_model = model;
            } else {
                self.default_agent_model = None;
                self.fallback_agent_model = None;
            }
            if let Some(default) = self.effective_default_agent_model().cloned() {
                for project in &mut self.projects {
                    for session in &mut project.sessions {
                        for pane in &mut session.panes {
                            if pane.chat.selected_model.is_none() {
                                pane.chat.selected_model = Some(default.clone());
                            }
                        }
                    }
                }
            }
            self.schedule_workspace_save(cx);
        }
        self.warm_agent_bridge(pane_id);
        self.close_agent_auth(pane_id, window, cx);
    }

    fn select_provider_for_auth(
        &mut self,
        pane_id: usize,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(auth) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.chat.auth.as_mut())
        else {
            return;
        };
        if let Some(mut login) = auth.login.take() {
            login.cancel();
        }
        let spec = crate::provider::get_provider(&provider);
        let primary_var = spec.and_then(|s| s.primary_env_var()).unwrap_or("API Key");
        let input_to_focus = auth.input.clone();
        auth.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_placeholder(format!("Paste {primary_var} here..."), window, cx);
            state.focus(window, cx);
        });
        auth.selected = Some(provider);
        auth.output.clear();
        auth.error = None;
        cx.notify();
        window.defer(cx, move |window, cx| {
            input_to_focus.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        });
    }

    fn start_agent_oauth(
        &mut self,
        pane_id: usize,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(auth) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.chat.auth.as_mut())
        else {
            return;
        };
        if let Some(mut login) = auth.login.take() {
            login.cancel();
        }
        auth.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_placeholder("Enter code or response if prompted...", window, cx);
            state.focus(window, cx);
        });
        auth.selected = Some(provider.clone());
        auth.output.clear();
        auth.error = None;
        match agent_auth::Login::start(&provider, &auth.cwd) {
            Ok(login) => auth.login = Some(login),
            Err(error) => auth.error = Some(error),
        }
        cx.notify();
    }

    fn start_agent_auth(
        &mut self,
        pane_id: usize,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = crate::provider::get_provider(&provider);
        let supports_oauth = spec.as_ref().map(|s| s.supports_oauth()).unwrap_or(false);
        let supports_api_key = spec.as_ref().map(|s| s.supports_api_key()).unwrap_or(true);

        if supports_oauth && !supports_api_key {
            self.start_agent_oauth(pane_id, provider, window, cx);
        } else {
            self.select_provider_for_auth(pane_id, provider, window, cx);
        }
    }

    fn submit_agent_auth(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(auth) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.chat.auth.as_mut())
        else {
            return;
        };
        if auth.page != AgentDialogPage::Auth {
            return;
        }

        // 1. If an interactive OAuth/PTY login is active, forward the response to it
        if let Some(login) = auth.login.as_mut() {
            let value = auth.input.read(cx).value().to_string();
            let result = login.submit(&value);
            auth.input
                .update(cx, |state, cx| state.set_value("", window, cx));
            if let Err(error) = result {
                auth.error = Some(error);
            }
            cx.notify();
            return;
        }

        // 2. If a provider is selected and input contains an API key, save it!
        if let Some(provider) = auth.selected.clone() {
            let key = auth.input.read(cx).value().trim().to_string();
            if key.is_empty() {
                auth.error = Some("API key cannot be empty".to_string());
                cx.notify();
                return;
            }

            match crate::provider::save_api_key(&provider, &key) {
                Ok(()) => {
                    auth.error = None;
                    auth.output = format!("Connected to {provider} successfully with API key!");
                    auth.input
                        .update(cx, |state, cx| state.set_value("", window, cx));

                    // Update live readiness in provider list
                    for p in &mut auth.providers {
                        if p.id == provider {
                            p.ready = true;
                            p.source = Some("Stored API key".to_string());
                        }
                    }

                    // Refresh models catalog
                    if let Ok(models) = agent_auth::available_models(&auth.cwd) {
                        auth.models = models;
                    }
                }
                Err(error) => {
                    auth.error = Some(format!("Could not save API key: {error}"));
                }
            }
            cx.notify();
            return;
        }

        // 3. Fallback: manual provider input
        let provider = auth.provider_input.read(cx).value().trim().to_string();
        if !provider.is_empty() {
            self.start_agent_auth(pane_id, provider, window, cx);
        }
    }

    fn disconnect_agent_provider(
        &mut self,
        pane_id: usize,
        provider: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let Some(auth) = pane.chat.auth.as_mut() else {
            return;
        };
        if let Some(mut login) = auth.login.take() {
            login.cancel();
        }

        match crate::provider::disconnect_provider(&provider) {
            Ok(()) => {
                auth.error = None;
                auth.output = format!("Disconnected from {provider}.");

                // Mark provider not ready
                for p in &mut auth.providers {
                    if p.id == provider {
                        p.ready = false;
                        p.source = None;
                    }
                }

                // If currently selected model belongs to this provider, clear it
                if pane
                    .chat
                    .selected_model
                    .as_ref()
                    .is_some_and(|m| m.provider == provider)
                {
                    pane.chat.selected_model = None;
                }

                // Refresh models
                if let Ok(models) = agent_auth::available_models(&auth.cwd) {
                    auth.models = models;
                }
            }
            Err(error) => {
                auth.error = Some(format!("Could not disconnect {provider}: {error}"));
            }
        }
        cx.notify();
    }

    fn poll_agent_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let mut authenticated = false;
        let mut streaming = false;
        let save_history = self.settings.restore.agent_history;
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if save_history && pane.chat.conversation.is_none() {
                        pane.chat.conversation = Some(conversations::Log::open(
                            &pane.uid,
                            &conversations::conversation_key(None),
                        ));
                    }
                    if let Some(auth) = pane.chat.auth.as_mut() {
                        if let Some(receiver) = &auth.discovery {
                            match receiver.try_recv() {
                                Ok(result) => {
                                    auth.discovery = None;
                                    match result {
                                        Ok(providers) => auth.providers = providers,
                                        Err(error) => auth.error = Some(error),
                                    }
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    auth.discovery = None;
                                    auth.error =
                                        Some("Provider discovery stopped unexpectedly.".into());
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            }
                        }
                        if let Some(receiver) = &auth.model_discovery {
                            match receiver.try_recv() {
                                Ok(result) => {
                                    auth.model_discovery = None;
                                    match result {
                                        Ok(models) => {
                                            auth.models = models;
                                            auth.model_error = None;
                                        }
                                        Err(error) => auth.model_error = Some(error),
                                    }
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    auth.model_discovery = None;
                                    auth.model_error =
                                        Some("Model discovery stopped unexpectedly.".into());
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            }
                        }
                        let mut finished = None;
                        if let Some(login) = auth.login.as_mut() {
                            while let Some(event) = login.try_event() {
                                changed = true;
                                match event {
                                    agent_auth::LoginEvent::Output(text) => {
                                        auth.output.push_str(&text)
                                    }
                                    agent_auth::LoginEvent::Finished(result) => {
                                        finished = Some(result);
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(result) = finished {
                            auth.login = None;
                            match result {
                                Ok(()) => {
                                    authenticated |=
                                        auth.selected.as_deref() != Some("google-antigravity");
                                    auth.output.push_str("\nAuthentication complete.");
                                    auth.discover();
                                }
                                Err(error) => auth.error = Some(error),
                            }
                        }
                    }
                    if let Some(mut bridge) = pane.chat.bridge.take() {
                        let mut closed = false;
                        for event in bridge.poll() {
                            changed = true;
                            let startup_error = pane.chat.messages.is_empty()
                                && !pane.chat.running
                                && matches!(event, agent_bridge::AgentEvent::Error(_));
                            if save_history && !startup_error {
                                if let Some(log) = pane.chat.conversation.as_mut() {
                                    if !matches!(
                                        &event,
                                        agent_bridge::AgentEvent::Done
                                            | agent_bridge::AgentEvent::Closed
                                    ) {
                                        log.append_event(&event);
                                    }
                                }
                            }
                            if pane.chat.selected_model.is_none() && pane.chat.autoscroll {
                                pane.chat.scroll.scroll_to_bottom();
                            }
                            match event {
                                agent_bridge::AgentEvent::Done => {
                                    pane.chat.running = false;
                                    finish_chat_tools(&mut pane.chat.messages);
                                    finish_chat_streaming(&mut pane.chat.messages);
                                }
                                agent_bridge::AgentEvent::Closed => {
                                    pane.chat.running = false;
                                    finish_chat_tools(&mut pane.chat.messages);
                                    finish_chat_streaming(&mut pane.chat.messages);
                                    closed = true;
                                }
                                agent_bridge::AgentEvent::Error(message) if startup_error => {
                                    pane.chat.startup_error = Some(message);
                                }
                                other => push_chat_event(&mut pane.chat.messages, other),
                            }
                        }
                        if let Some(id) = bridge.session_id() {
                            pane.chat.rpi_session_id = Some(id);
                        }
                        if closed {
                            pane.chat.cwd = None;
                        } else {
                            pane.chat.bridge = Some(bridge);
                        }
                    }
                    streaming |= pane.chat.running;
                    let pane_uid = pane.uid.clone();
                    for (model, chat) in &mut pane.chat.models {
                        if save_history && chat.conversation.is_none() {
                            chat.conversation = Some(conversations::Log::open(
                                &pane_uid,
                                &conversations::conversation_key(Some(model)),
                            ));
                        }
                        if let Some(mut bridge) = chat.bridge.take() {
                            let mut closed = false;
                            for event in bridge.poll() {
                                changed = true;
                                let startup_error = chat.messages.is_empty()
                                    && !chat.running
                                    && matches!(event, agent_bridge::AgentEvent::Error(_));
                                if save_history && !startup_error {
                                    if let Some(log) = chat.conversation.as_mut() {
                                        if !matches!(
                                            &event,
                                            agent_bridge::AgentEvent::Done
                                                | agent_bridge::AgentEvent::Closed
                                        ) {
                                            log.append_event(&event);
                                        }
                                    }
                                }
                                if pane.chat.selected_model.as_ref() == Some(model)
                                    && pane.chat.autoscroll
                                {
                                    pane.chat.scroll.scroll_to_bottom();
                                }
                                match event {
                                    agent_bridge::AgentEvent::Done => {
                                        chat.running = false;
                                        finish_chat_tools(&mut chat.messages);
                                        finish_chat_streaming(&mut chat.messages);
                                    }
                                    agent_bridge::AgentEvent::Closed => {
                                        chat.running = false;
                                        finish_chat_tools(&mut chat.messages);
                                        finish_chat_streaming(&mut chat.messages);
                                        closed = true;
                                    }
                                    agent_bridge::AgentEvent::Error(message) if startup_error => {
                                        chat.startup_error = Some(message);
                                    }
                                    other => push_chat_event(&mut chat.messages, other),
                                }
                            }
                            if let ModelBridge::Rpi(bridge) = &bridge {
                                if let Some(id) = bridge.session_id() {
                                    chat.rpi_session_id = Some(id);
                                }
                            }
                            if !closed {
                                chat.bridge = Some(bridge);
                            }
                        }
                        streaming |= chat.running;
                    }
                }
            }
        }
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if authenticated && pane.chat.bridge.is_some() {
                        pane.chat.auth_refresh_pending = true;
                    }
                    if pane.chat.auth_refresh_pending && !pane.chat.running {
                        pane.chat.auth_refresh_pending = false;
                        if let Some(bridge) = pane.chat.bridge.as_mut() {
                            if let Err(error) = bridge.restart_after_auth() {
                                pane.chat
                                    .messages
                                    .push(ChatMessage::new(ChatRole::Error, error));
                            }
                        }
                        changed = true;
                    }
                    for chat in pane.chat.models.values_mut() {
                        if authenticated && matches!(chat.bridge, Some(ModelBridge::Rpi(_))) {
                            chat.auth_refresh_pending = true;
                        }
                        if chat.auth_refresh_pending && !chat.running {
                            chat.auth_refresh_pending = false;
                            if let Some(ModelBridge::Rpi(bridge)) = chat.bridge.as_mut() {
                                if let Err(error) = bridge.restart_after_auth() {
                                    chat.messages.push(ChatMessage::new(ChatRole::Error, error));
                                }
                            }
                            changed = true;
                        }
                    }
                }
            }
        }
        if changed {
            cx.notify();
        }
        streaming
    }

    fn submit_agent_prompt(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let save_history = self.settings.restore.agent_history;
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let cwd = self
            .pane_cwd_path(pane)
            .unwrap_or_else(|| PathBuf::from("."));
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let Some(input) = pane.input_state.clone() else {
            return;
        };
        let page = match input.read(cx).value().trim() {
            "/auth" => Some(AgentDialogPage::Auth),
            "/model" => Some(AgentDialogPage::Models),
            _ => None,
        };
        if let Some(page) = page {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            self.open_agent_auth(pane_id, page, window, cx);
            return;
        }
        let mut prompt = input.read(cx).value().to_string();
        for attachment in &pane.attachments {
            if !prompt.is_empty() {
                prompt.push(' ');
            }
            prompt.push_str(&attachments::prompt_path(&attachment.path));
        }
        let trimmed = prompt.trim();
        if trimmed.is_empty() || pane.chat.visible_running() {
            return;
        }
        if trimmed == "/fast" {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            self.toggle_fast_mode(pane_id, cx);
            let fast = self
                .pane_ref(pane_id)
                .map(|p| p.chat.fast_mode)
                .unwrap_or(true);
            let msg = if fast {
                "⚡ Fast mode enabled (low reasoning for rapid responses)."
            } else {
                "🧠 Deep mode enabled (full reasoning for complex logic)."
            };
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.chat
                    .visible_messages_mut()
                    .push(ChatMessage::new(ChatRole::Status, msg.into()));
                if let Some(log) = pane.chat.visible_conversation() {
                    log.append_local(ChatRole::Status, msg);
                }
            }
            cx.notify();
            return;
        }
        if let Some(arg) = trimmed.strip_prefix("/thinking") {
            let level = match arg.trim() {
                "off" => "off",
                "min" | "minimal" => "minimal",
                "low" => "low",
                "med" | "medium" => "medium",
                "high" | "max" => "high",
                _ => "low",
            };
            input.update(cx, |state, cx| state.set_value("", window, cx));
            let fast = matches!(level, "off" | "minimal" | "low");
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.chat.fast_mode = fast;
                if let Some(model) = &pane.chat.selected_model {
                    if let Some(chat) = pane.chat.models.get_mut(model) {
                        if let Some(bridge) = chat.bridge.as_mut() {
                            let _ = bridge.set_thinking_level(level);
                        }
                    }
                }
                pane.chat.visible_messages_mut().push(ChatMessage::new(
                    ChatRole::Status,
                    format!(
                        "Thinking level set to '{level}' ({} mode).",
                        if fast { "Fast" } else { "Deep" }
                    ),
                ));
                let status = format!(
                    "Thinking level set to '{level}' ({} mode).",
                    if fast { "Fast" } else { "Deep" }
                );
                if let Some(log) = pane.chat.visible_conversation() {
                    log.append_local(ChatRole::Status, &status);
                }
            }
            cx.notify();
            return;
        }
        if let Some(model) = pane.chat.selected_model.clone() {
            let fast = pane.chat.fast_mode;
            let pane_uid = pane.uid.clone();
            let model_key = conversations::conversation_key(Some(&model));
            let chat = pane.chat.models.entry(model.clone()).or_default();
            if save_history && chat.conversation.is_none() {
                chat.conversation = Some(conversations::Log::open(&pane_uid, &model_key));
            }
            if chat.bridge.is_none() {
                let bridge = match model.backend {
                    agent_auth::ModelBackend::Rpi => {
                        let selector = format!("{}/{}", model.provider, model.id);
                        agent_bridge::AgentBridge::spawn_resumed(
                            &cwd,
                            chat.rpi_session_id.as_deref(),
                            Some(&selector),
                        )
                        .or_else(|_| agent_bridge::AgentBridge::spawn_model(&cwd, Some(&selector)))
                        .map(ModelBridge::Rpi)
                    }
                    agent_auth::ModelBackend::Omp => {
                        omp_bridge::OmpBridge::spawn(&cwd, &model.id).map(ModelBridge::Omp)
                    }
                };
                match bridge {
                    Ok(mut bridge) => {
                        if !fast {
                            let _ = bridge.set_thinking_level("high");
                        }
                        chat.bridge = Some(bridge);
                    }
                    Err(error) => {
                        chat.messages.push(ChatMessage::new(ChatRole::Error, error));
                        cx.notify();
                        return;
                    }
                }
            }
            chat.startup_error = None;
            match chat.bridge.as_mut().unwrap().send_prompt(&prompt) {
                Ok(()) => {
                    if let Some(log) = chat.conversation.as_mut() {
                        log.append_user(&prompt);
                    }
                    chat.messages.push(ChatMessage::new(ChatRole::User, prompt));
                    chat.running = true;
                    pane.attachments.clear();
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                }
                Err(error) => chat.messages.push(ChatMessage::new(ChatRole::Error, error)),
            }
            pane.chat.autoscroll = true;
            pane.chat.scroll.scroll_to_bottom();
            cx.notify();
            return;
        }
        pane.chat.startup_error = None;
        if save_history && pane.chat.conversation.is_none() {
            pane.chat.conversation = Some(conversations::Log::open(
                &pane.uid,
                &conversations::conversation_key(None),
            ));
        }
        if pane.chat.bridge.is_none() {
            let saved_session = pane.chat.rpi_session_id.clone();
            match agent_bridge::AgentBridge::spawn_resumed(&cwd, saved_session.as_deref(), None)
                .or_else(|_| agent_bridge::AgentBridge::spawn(&cwd))
            {
                Ok(bridge) => {
                    pane.chat.cwd = Some(cwd);
                    pane.chat.bridge = Some(bridge);
                }
                Err(error) => {
                    pane.chat
                        .messages
                        .push(ChatMessage::new(ChatRole::Error, error));
                    cx.notify();
                    return;
                }
            }
        }
        match pane.chat.bridge.as_mut().unwrap().send_prompt(&prompt) {
            Ok(()) => {
                if let Some(log) = pane.chat.conversation.as_mut() {
                    log.append_user(&prompt);
                }
                pane.chat
                    .messages
                    .push(ChatMessage::new(ChatRole::User, prompt));
                pane.chat.running = true;
                pane.attachments.clear();
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            Err(error) => pane
                .chat
                .messages
                .push(ChatMessage::new(ChatRole::Error, error)),
        }
        pane.chat.autoscroll = true;
        pane.chat.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn execute_pane_input(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            self.submit_agent_prompt(pane_id, window, cx);
            return;
        }
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        let Some(input_state) = pane.input_state.clone() else {
            return;
        };

        let raw = input_state.read(cx).value().to_string();
        let has_attachments = !pane.attachments.is_empty();

        if raw.is_empty() && !has_attachments {
            pane.terminal.update(cx, |term, cx| {
                term.send_bytes(b"\r", cx);
            });
        } else {
            let terminal = pane.terminal.clone();

            if has_attachments {
                // Deliver image paths with real paste semantics. Agent TUIs
                // (Claude Code, Codex) auto-attach an image path that arrives
                // as a paste, but show it as literal text when it is typed.
                let paths: Vec<String> = pane
                    .attachments
                    .iter()
                    .map(|attachment| attachments::prompt_path(&attachment.path))
                    .collect();
                let prompt = raw.clone();
                App::spawn(cx, async move |cx: &mut gpui::AsyncApp| {
                    if !prompt.is_empty() {
                        let text = format!("{prompt} ");
                        let _ = terminal.update(cx, |term, cx| {
                            term.send_text(&text, cx);
                        });
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(30))
                            .await;
                    }
                    for path in paths {
                        let _ = terminal.update(cx, |term, cx| {
                            term.paste_text_from_app(&path, cx);
                        });
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(100))
                            .await;
                    }
                    // Give the TUI time to finish reading and attaching the
                    // images before the submitting Enter arrives.
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(300))
                        .await;
                    let _ = terminal.update(cx, |term, cx| {
                        term.send_bytes(b"\r", cx);
                    });
                })
                .detach();
            } else {
                terminal.update(cx, |term, cx| {
                    term.send_text(&raw, cx);
                });

                // Send Enter separately after a brief tick so raw-mode CLI agents
                // (Claude Code, Ink, readline) receive it as an authentic Enter
                // keystroke rather than pasted text, triggering immediate
                // automatic prompt submission.
                let terminal = terminal.clone();
                App::spawn(cx, async move |cx: &mut gpui::AsyncApp| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(25))
                        .await;
                    let _ = terminal.update(cx, |term, cx| {
                        term.send_bytes(b"\r", cx);
                    });
                })
                .detach();
            }

            if pane.command_history.last().map(|s| s.as_str()) != Some(&raw) && !raw.is_empty() {
                pane.command_history.push(raw);
            }
            pane.history_index = None;
            pane.pending_draft.clear();
            pane.attachments.clear();
            input_state.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        input_state.update(cx, |state, cx| {
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// Transfer input ownership between the editor and the command's PTY.
    fn on_shell_event(&mut self, pane_id: usize, event: &ShellEvent, cx: &mut Context<Self>) {
        let mut cwd_changed = false;
        {
            let Some(pane) = self.pane_mut(pane_id) else {
                return;
            };
            match event {
                ShellEvent::CommandStarted { .. } => {
                    pane.command_running = true;
                    pane.completion_menu = None;
                }
                ShellEvent::CommandFinished { .. } | ShellEvent::PromptStart => {
                    pane.command_running = false;
                    pane.bottom_input_visible = true;
                }
                ShellEvent::CwdChanged(path) => {
                    pane.cwd = Some(PathBuf::from(path));
                    cwd_changed = true;
                }
                _ => return,
            }
            let show_input = pane.should_show_bottom_input(cx);
            pane.terminal
                .update(cx, |terminal, _| terminal.set_read_only(show_input));
        }
        if cwd_changed {
            self.mark_dirty(cx);
        }
        cx.notify();
    }

    fn handle_pane_ctrl_c(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        if pane.chat.active {
            if let Some(model) = pane.chat.selected_model.as_ref() {
                if let Some(chat) = pane.chat.models.get_mut(model) {
                    if chat.running {
                        if let Some(bridge) = chat.bridge.as_mut() {
                            if let Err(error) = bridge.cancel() {
                                chat.messages.push(ChatMessage::new(ChatRole::Error, error));
                            }
                        }
                    } else if let Some(input) = &pane.input_state {
                        input.update(cx, |state, cx| state.set_value("", window, cx));
                    }
                } else if let Some(input) = &pane.input_state {
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                }
                cx.notify();
                return;
            }
            if pane.chat.running {
                if let Some(bridge) = pane.chat.bridge.as_mut() {
                    if let Err(error) = bridge.cancel() {
                        pane.chat
                            .messages
                            .push(ChatMessage::new(ChatRole::Error, error));
                    }
                }
            } else if let Some(input) = &pane.input_state {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            cx.notify();
            return;
        }

        // 1. Clear input state and draft
        if let Some(input_state) = &pane.input_state {
            input_state.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        pane.history_index = None;
        pane.pending_draft.clear();
        pane.completion_menu = None;

        // 2. Send \x03 (ETX/SIGINT) to PTY (interrupts process on Linux, macOS, and Windows ConPTY)
        pane.terminal.update(cx, |term, cx| {
            term.send_bytes(b"\x03", cx);
        });

        // 3. On Unix: also directly signal the active foreground process group
        #[cfg(unix)]
        if let Some(pid) = pane.pid {
            if let Some(tpgid) = status::read_proc_tpgid(pid) {
                if tpgid > 0 && tpgid != pid as i64 {
                    unsafe {
                        libc::kill(-(tpgid as libc::pid_t), libc::SIGINT);
                    }
                }
            }
        }

        // 4. If an agent/command was working, interrupt and return to Idle
        if pane.status == SessionStatus::Working {
            pane.status = SessionStatus::Idle;
            pane.bottom_input_visible = true;
            pane.terminal.update(cx, |term, _| term.set_read_only(true));
        }

        cx.notify();
    }

    fn handle_pane_input_key(
        &mut self,
        pane_id: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let key = keystroke.key.as_str();
        let modifiers = keystroke.modifiers;

        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.chat.auth.is_some() {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                self.close_agent_auth(pane_id, window, cx);
            }
            return;
        }
        let Some(input_state) = pane.input_state.clone() else {
            return;
        };
        let value = input_state.read(cx).value().to_string();
        let cursor_utf16 = input_state.read(cx).cursor();
        let cursor_byte = completion::utf16_to_byte(&value, cursor_utf16);
        let cursor_line = input_state.read(cx).cursor_position().line;
        let last_line = value.matches('\n').count();
        let has_menu = pane.completion_menu.is_some();
        let plus_menu_open = pane.plus_menu_open;
        let terminal = pane.terminal.clone();

        // --- completion menu navigation ------------------------------------
        if has_menu {
            if key == "down" {
                cx.stop_propagation();
                self.move_completion_selection(pane_id, 1, cx);
                return;
            }
            if key == "up" {
                cx.stop_propagation();
                self.move_completion_selection(pane_id, -1, cx);
                return;
            }
            if key == "right" && !modifiers.control && !modifiers.shift && !modifiers.alt {
                cx.stop_propagation();
                let selected = self
                    .pane_ref(pane_id)
                    .and_then(|pane| pane.completion_menu.as_ref().map(|menu| menu.selected))
                    .unwrap_or(0);
                self.accept_completion_at(pane_id, selected, window, cx);
                return;
            }
            if (key == "enter" || key == "return") && !modifiers.shift {
                // Enter means "run this", never "insert the highlighted
                // suggestion": Tab and Right accept completions.
                cx.stop_propagation();
                self.close_completion_menu(pane_id, cx);
                self.execute_pane_input(pane_id, window, cx);
                return;
            }
            if key == "tab" && !modifiers.control && !modifiers.alt {
                cx.stop_propagation();
                let selected = self
                    .pane_ref(pane_id)
                    .and_then(|pane| pane.completion_menu.as_ref().map(|menu| menu.selected))
                    .unwrap_or(0);
                self.accept_completion_at(pane_id, selected, window, cx);
                return;
            }
            if key == "escape" {
                cx.stop_propagation();
                self.close_completion_menu(pane_id, cx);
                return;
            }
        }

        // Enter: execute the command.
        if (key == "enter" || key == "return")
            && !modifiers.shift
            && !modifiers.control
            && !modifiers.alt
        {
            cx.stop_propagation();
            self.execute_pane_input(pane_id, window, cx);
            return;
        }

        // Shift+Enter: insert a newline (multi-line commands, Kathamo style).
        if (key == "enter" || key == "return")
            && modifiers.shift
            && !modifiers.control
            && !modifiers.alt
        {
            input_state.update(cx, |state, cx| {
                state.insert("\n", window, cx);
            });
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+C is handled by the terminal's `Copy` action as well; the guard
        // here keeps it working even when the editor has no selection.
        if key == "c" && modifiers.control && !modifiers.shift && !modifiers.alt {
            cx.stop_propagation();
            self.handle_pane_ctrl_c(pane_id, window, cx);
            return;
        }
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            return;
        }

        // Ctrl+R: reverse history search, like the shell's own prompt.
        if key == "r" && modifiers.control && !modifiers.shift && !modifiers.alt {
            cx.stop_propagation();
            self.open_history_search(pane_id, window, cx);
            return;
        }

        // Tab: accept the ghost suggestion, or open completion candidates.
        if key == "tab" && !modifiers.control && !modifiers.alt {
            cx.stop_propagation();
            self.start_completion(pane_id, window, cx);
            return;
        }

        // Right at the end of the line accepts a ghost suggestion.
        if key == "right" && !modifiers.control && !modifiers.shift && !modifiers.alt {
            if cursor_utf16 >= completion::utf16_len(&value)
                && input_state.read(cx).has_inline_completion()
            {
                input_state.update(cx, |state, cx| {
                    state.accept_inline_completion(window, cx);
                });
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }

        // Ctrl+A / Ctrl+E: readline start/end of line.
        if key == "a" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let start = value[..cursor_byte].rfind('\n').map_or(0, |idx| idx + 1);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "e" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let end = value[cursor_byte..]
                .find('\n')
                .map_or(value.len(), |idx| cursor_byte + idx);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, end),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+U: kill from the start of the line to the cursor.
        if key == "u" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let start = value[..cursor_byte].rfind('\n').map_or(0, |idx| idx + 1);
            let mut new_value = String::with_capacity(value.len());
            new_value.push_str(&value[..start]);
            new_value.push_str(&value[cursor_byte..]);
            set_input_text_with_cursor(
                &input_state,
                new_value,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.history_index = None;
                pane.pending_draft.clear();
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+W / Alt+Backspace: delete the previous word.
        if (key == "w" && modifiers.control && !modifiers.shift && !modifiers.alt)
            || (key == "backspace" && modifiers.alt && !modifiers.control && !modifiers.shift)
        {
            let start = previous_word_start(&value, cursor_byte);
            let mut new_value = String::with_capacity(value.len());
            new_value.push_str(&value[..start]);
            new_value.push_str(&value[cursor_byte..]);
            set_input_text_with_cursor(
                &input_state,
                new_value,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+K: kill from the cursor to the end of the line.
        if key == "k" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let end = value[cursor_byte..]
                .find('\n')
                .map_or(value.len(), |idx| cursor_byte + idx);
            let mut new_value = String::with_capacity(value.len());
            new_value.push_str(&value[..cursor_byte]);
            new_value.push_str(&value[end..]);
            set_input_text_with_cursor(&input_state, new_value, cursor_utf16, window, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Alt+B / Alt+F: move by word (readline).
        if key == "b" && modifiers.alt && !modifiers.control && !modifiers.shift {
            let start = previous_word_start(&value, cursor_byte);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "f" && modifiers.alt && !modifiers.control && !modifiers.shift {
            let end = next_word_end(&value, cursor_byte);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, end),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+L: clear the terminal screen.
        if key == "l" && modifiers.control && !modifiers.shift && !modifiers.alt {
            terminal.update(cx, |term, cx| {
                term.send_bytes(b"\x0c", cx);
            });
            cx.stop_propagation();
            return;
        }

        // Ctrl+Z stays with the editor (undo), not job control: a terminal
        // prompt's line editor is what the user is typing into here.

        // Ctrl+D: EOF on an empty line.
        if key == "d" && modifiers.control && !modifiers.shift && !modifiers.alt {
            if value.is_empty() {
                terminal.update(cx, |term, cx| {
                    term.send_bytes(b"\x04", cx);
                });
                cx.stop_propagation();
                return;
            }
        }

        // Ctrl+\: quit the foreground job (SIGQUIT).
        if key == "\\" && modifiers.control && !modifiers.shift && !modifiers.alt {
            terminal.update(cx, |term, cx| {
                term.send_bytes(b"\x1c", cx);
            });
            cx.stop_propagation();
            return;
        }

        // Escape: dismiss menus, then clear the input.
        if key == "escape" && !modifiers.shift && !modifiers.control && !modifiers.alt {
            if self.terminal_context_menu.is_some() {
                self.terminal_context_menu = None;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if plus_menu_open {
                if let Some(pane) = self.pane_mut(pane_id) {
                    pane.plus_menu_open = false;
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if !value.is_empty() {
                input_state.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                });
                if let Some(pane) = self.pane_mut(pane_id) {
                    pane.history_index = None;
                    pane.pending_draft.clear();
                }
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }

        // Ctrl+P / Ctrl+N: previous / next history entry.
        if (key == "p" || key == "n") && modifiers.control && !modifiers.shift && !modifiers.alt {
            cx.stop_propagation();
            self.step_pane_history(pane_id, key == "p", window, cx);
            return;
        }

        // Up / Down: history, but only once the cursor is on the first / last
        // line of a multi-line buffer, so line navigation still works.
        if key == "up" && !modifiers.shift && !modifiers.control && !modifiers.alt {
            if cursor_line == 0 {
                cx.stop_propagation();
                self.step_pane_history(pane_id, true, window, cx);
            }
            return;
        }
        if key == "down" && !modifiers.shift && !modifiers.control && !modifiers.alt {
            if cursor_line as usize >= last_line {
                cx.stop_propagation();
                self.step_pane_history(pane_id, false, window, cx);
            }
            return;
        }

        // Ordinary typing leaves history navigation mode.
        if (!modifiers.control && !modifiers.alt)
            && (keystroke.key_char.is_some() || key == "backspace" || key == "delete")
        {
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.history_index = None;
            }
        }
    }

    fn ensure_panes_input_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut missing = Vec::new();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    let placeholder = "Run commands";
                    if let Some(input) = &pane.input_state {
                        input.update(cx, |state, cx| {
                            state.set_placeholder(placeholder, window, cx);
                            state.set_auto_grow(1, 8);
                        });
                        if pane.input_subscription.is_none() && pane.input_state.is_none() {
                            missing.push(pane.id);
                        }
                    } else {
                        missing.push(pane.id);
                    }
                }
            }
        }
        for pane_id in missing {
            self.create_pane_input(pane_id, window, cx);
        }

        // Dynamically ensure the focused pane's input is focused.
        // The history palette owns focus while it is open.
        if self.active_view == ActiveView::Workspace && self.history_search.is_none() {
            if let Some(pane_id) = self.focused_pane_id {
                if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) {
                    if let Some(project) = self.projects.get(p_idx) {
                        if let Some(session) = project.sessions.get(s_idx) {
                            if let Some(pane) = session.panes.get(pane_idx) {
                                if pane.chat.auth.is_some() {
                                    return;
                                }
                                let should_show = pane.should_show_bottom_input(cx);
                                pane.terminal.update(cx, |term, _| {
                                    term.set_read_only(should_show);
                                });
                                if should_show {
                                    if let Some(input_state) = &pane.input_state {
                                        let is_input_focused =
                                            input_state.read(cx).focus_handle().is_focused(window);
                                        let is_term_focused = pane
                                            .terminal
                                            .read(cx)
                                            .focus_handle()
                                            .is_focused(window);
                                        if is_term_focused || !is_input_focused {
                                            input_state.update(cx, |state, cx| {
                                                state.focus(window, cx);
                                            });
                                        }
                                    }
                                } else {
                                    let is_term_focused =
                                        pane.terminal.read(cx).focus_handle().is_focused(window);
                                    if !is_term_focused {
                                        pane.terminal
                                            .read(cx)
                                            .focus_handle()
                                            .clone()
                                            .focus(window, cx);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn open_terminal_context_menu(
        &mut self,
        pane_id: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal_context_menu = Some(TerminalContextMenu { pane_id, position });
        self.focus_pane(pane_id, window, cx);
        cx.notify();
    }

    fn close_terminal_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.terminal_context_menu.is_some() {
            self.terminal_context_menu = None;
            cx.notify();
        }
    }

    fn copy_terminal_selection(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) {
            if let Some(pane) = self
                .projects
                .get(p_idx)
                .and_then(|p| p.sessions.get(s_idx))
                .and_then(|s| s.panes.get(pane_idx))
            {
                if let Some(text) = pane.terminal.read(cx).selected_text() {
                    if !text.is_empty() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        pane.terminal.update(cx, |term, cx| {
                            term.clear_selection(cx);
                        });
                    }
                }
            }
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    fn paste_from_clipboard_into_pane(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = cx.read_from_clipboard() {
            // Clipboard images and copied image paths become attachments
            // instead of pasting either a path or nothing.
            if self.attach_images_from_item(pane_id, &item, window, cx) {
                self.close_terminal_context_menu(cx);
                return;
            }

            if let Some(text) = item.text() {
                let clean = text
                    .trim_matches(&['\r', '\n'][..])
                    .replace("\r\n", " ")
                    .replace('\n', " ")
                    .replace('\r', "");
                if !clean.is_empty() {
                    let Some(pane) = self.pane_ref(pane_id) else {
                        return;
                    };
                    if pane.should_show_bottom_input(cx) {
                        if let Some(input_state) = &pane.input_state {
                            input_state.update(cx, |state, cx| {
                                let cur = state.value().to_string();
                                state.set_value(&format!("{cur}{clean}"), window, cx);
                                state.focus(window, cx);
                            });
                        }
                    } else {
                        pane.terminal.update(cx, |term, cx| {
                            term.paste_text(&clean, cx);
                        });
                    }
                }
            }
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    /// Stage one image on a pane's input bar and reveal the bar so the user
    /// can see what will be sent.
    fn attach_image(
        &mut self,
        pane_id: usize,
        attachment: attachments::AttachedImage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self.pane_mut(pane_id) else {
            return false;
        };
        if !attachments::push_unique(&mut pane.attachments, attachment) {
            return false;
        }
        let input_state = pane.input_state.clone();

        self.focused_pane_id = Some(pane_id);
        if let Some(input_state) = input_state {
            input_state.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
        true
    }

    /// Ctrl/Cmd+V with an image on the clipboard attaches it. Returns false
    /// when the clipboard holds no image so the editor's normal text paste
    /// can run.
    fn attach_image_from_clipboard(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        self.attach_images_from_item(pane_id, &item, window, cx)
    }

    /// Attach clipboard pixel data, or fall back to a copied image path.
    /// File managers often place a path (`/tmp/shot.png` or `file:///tmp/shot.png`)
    /// on the clipboard instead of the pixels, which used to be pasted as text.
    fn attach_images_from_item(
        &mut self,
        pane_id: usize,
        item: &gpui::ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(image) = item.entries().iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(image),
            ClipboardEntry::String(_) | ClipboardEntry::ExternalPaths(_) => None,
        }) {
            if let Some(attachment) = attachments::from_clipboard_image(image) {
                return self.attach_image(pane_id, attachment, window, cx);
            }
        }

        let Some(text) = item.text() else {
            return false;
        };
        // Only reinterpret copied paths inside agent panes: in a shell a
        // copied path is usually meant as literal text for a command.
        let agent_running = self.pane_ref(pane_id).is_some_and(|pane| {
            pane.is_agent
                || pane.agent_active
                || status::foreground_process_names(pane.pid)
                    .iter()
                    .any(|name| status::is_agent_process(name))
        });
        if !agent_running {
            return false;
        }

        let mut found = attachments::from_clipboard_text(&text);
        if found.is_empty() {
            return false;
        }

        let mut attached = false;
        while let Some(attachment) = found.pop() {
            attached |= self.attach_image(pane_id, attachment, window, cx);
        }
        attached
    }

    /// Handle files dropped anywhere on the window. Only image files are
    /// consumed; everything else falls through to the platform.
    fn drop_external_images(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let pane_id = self.focused_pane_id.or_else(|| {
            self.active_session()
                .and_then(|session| session.panes.first().map(|pane| pane.id))
        });
        let Some(pane_id) = pane_id else {
            return false;
        };

        let mut attached = false;
        for path in paths.paths() {
            if let Some(attachment) = attachments::from_path(path) {
                attached |= self.attach_image(pane_id, attachment, window, cx);
            }
        }
        attached
    }

    fn remove_attachment(&mut self, pane_id: usize, index: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id)
            && index < pane.attachments.len()
        {
            pane.attachments.remove(index);
            cx.notify();
        }
    }

    fn open_image_viewer(
        &mut self,
        attachment: &attachments::AttachedImage,
        cx: &mut Context<Self>,
    ) {
        self.image_viewer = Some(ImageViewerState {
            path: attachment.path.clone(),
            name: attachment.name.clone(),
            dimensions: attachment.dimensions(),
        });
        cx.notify();
    }

    fn select_all_in_terminal(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) {
            if let Some(pane) = self
                .projects
                .get(p_idx)
                .and_then(|p| p.sessions.get(s_idx))
                .and_then(|s| s.panes.get(pane_idx))
            {
                pane.terminal.update(cx, |term, cx| {
                    term.select_all(cx);
                });
            }
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    fn clear_terminal_buffer(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) {
            if let Some(pane) = self
                .projects
                .get(p_idx)
                .and_then(|p| p.sessions.get(s_idx))
                .and_then(|s| s.panes.get(pane_idx))
            {
                pane.terminal.update(cx, |term, cx| {
                    term.send_bytes(b"\x0c", cx);
                });
            }
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    fn render_terminal_context_menu_layer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(menu) = self.terminal_context_menu else {
            return div().into_any_element();
        };

        let pane_id = menu.pane_id;
        let tokens = self.tokens(cx);

        let has_selection = self
            .pane_index
            .get(&pane_id)
            .and_then(|&(p_idx, s_idx, pane_idx)| {
                self.projects
                    .get(p_idx)
                    .and_then(|p| p.sessions.get(s_idx))
                    .and_then(|s| s.panes.get(pane_idx))
            })
            .map(|pane| pane.terminal.read(cx).has_selection())
            .unwrap_or(false);

        let left = (menu.position.x - px(5.0)).max(px(10.0));
        let top = (menu.position.y - px(5.0)).max(px(35.0));

        let modal_border: Hsla = if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };
        let modal_bg: Hsla = if tokens.is_dark {
            rgba(0x16171eff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let sep_bg: Hsla = if tokens.is_dark {
            rgba(0xffffff14).into()
        } else {
            rgba(0x00000014).into()
        };

        deferred(
            div()
                .id("terminal-context-menu-layer")
                .size_full()
                .absolute()
                .top_0()
                .left_0()
                .child(
                    div()
                        .id("terminal-context-menu-backdrop")
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.close_terminal_context_menu(cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, _, _, cx| {
                                this.close_terminal_context_menu(cx);
                            }),
                        ),
                )
                .child(
                    div()
                        .id(("terminal-context-menu-modal", pane_id))
                        .absolute()
                        .top(top)
                        .left(left)
                        .w(px(180.0))
                        .p(px(5.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(modal_border)
                        .bg(modal_bg)
                        .shadow_lg()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        // Copy option
                        .child(
                            div()
                                .id(("context-menu-copy", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(if has_selection {
                                    CursorStyle::PointingHand
                                } else {
                                    CursorStyle::Arrow
                                })
                                .opacity(if has_selection { 1.0 } else { 0.45 })
                                .when(has_selection, |this| {
                                    this.hover(|s| s.bg(tokens.sidebar_item_hover))
                                })
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            svg()
                                                .path("icons/copy.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.text_primary),
                                        )
                                        .child(
                                            div()
                                                .font_family(MONO_FONT)
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_primary)
                                                .child("Copy"),
                                        ),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+C"),
                                )
                                .when(has_selection, |this| {
                                    this.on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.copy_terminal_selection(pane_id, window, cx);
                                        }),
                                    )
                                }),
                        )
                        // Paste option
                        .child(
                            div()
                                .id(("context-menu-paste", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(CursorStyle::PointingHand)
                                .hover(|s| s.bg(tokens.sidebar_item_hover))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            svg()
                                                .path("icons/clipboard.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.text_primary),
                                        )
                                        .child(
                                            div()
                                                .font_family(MONO_FONT)
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_primary)
                                                .child("Paste"),
                                        ),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+V"),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.paste_from_clipboard_into_pane(pane_id, window, cx);
                                    }),
                                ),
                        )
                        // Separator
                        .child(div().my(px(2.0)).h(px(1.0)).w_full().bg(sep_bg))
                        // Select All
                        .child(
                            div()
                                .id(("context-menu-select-all", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(CursorStyle::PointingHand)
                                .hover(|s| s.bg(tokens.sidebar_item_hover))
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.0))
                                        .text_color(tokens.text_primary)
                                        .child("Select All"),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+A"),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.select_all_in_terminal(pane_id, window, cx);
                                    }),
                                ),
                        )
                        // Clear Screen
                        .child(
                            div()
                                .id(("context-menu-clear", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(CursorStyle::PointingHand)
                                .hover(|s| s.bg(tokens.sidebar_item_hover))
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.0))
                                        .text_color(tokens.text_primary)
                                        .child("Clear Screen"),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+L"),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.clear_terminal_buffer(pane_id, window, cx);
                                    }),
                                ),
                        ),
                ),
        )
        .priority(200)
        .into_any_element()
    }

    /// Ctrl+R palette: type to filter, arrows to pick, Enter to insert.
    fn render_history_search_layer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(search) = self.history_search.as_ref() else {
            return div().into_any_element();
        };
        let tokens = self.tokens(cx);

        let panel_bg: Hsla = if tokens.is_dark {
            rgba(0x16171eff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let panel_border: Hsla = if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };

        let row_selected_bg: Hsla = if tokens.is_dark {
            rgba(0x262838ff).into()
        } else {
            rgba(0x0000000f).into()
        };
        let row_hover_bg: Hsla = if tokens.is_dark {
            rgba(0x1f2130ff).into()
        } else {
            rgba(0x0000000a).into()
        };

        let rows: Vec<gpui::AnyElement> = if search.matches.is_empty() {
            vec![
                div()
                    .px(px(10.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(tokens.text_muted)
                    .child("No matching commands")
                    .into_any_element(),
            ]
        } else {
            search
                .matches
                .iter()
                .take(10)
                .enumerate()
                .map(|(index, command)| {
                    let selected = index == search.selected;
                    div()
                        .id(("history-search-row", index))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(10.0))
                        .px(px(9.0))
                        .py(px(6.0))
                        .rounded(px(5.0))
                        .cursor(CursorStyle::PointingHand)
                        .when(selected, |this| this.bg(row_selected_bg))
                        .hover(|this| this.bg(row_hover_bg))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .min_w_0()
                                .child(
                                    div()
                                        .w(px(16.0))
                                        .text_size(px(10.0))
                                        .text_color(if selected {
                                            rgba(0x38bdf8ff).into()
                                        } else {
                                            tokens.text_muted
                                        })
                                        .child(format!("{}", index + 1)),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.5))
                                        .text_color(tokens.text_primary)
                                        .truncate()
                                        .child(command.clone()),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(tokens.text_muted)
                                .child("↵"),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.accept_history_search_index(index, window, cx);
                            }),
                        )
                        .into_any_element()
                })
                .collect()
        };

        deferred(
            div()
                .id("history-search-layer")
                .size_full()
                .absolute()
                .top_0()
                .left_0()
                .child(
                    div()
                        .id("history-search-backdrop")
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0()
                        .bg(rgba(0x00000055))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_history_search(window, cx);
                            }),
                        ),
                )
                .child(
                    div()
                        .id("history-search-panel")
                        .absolute()
                        .top(px(80.0))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w(px(560.0))
                                .p(px(10.0))
                                .rounded(px(10.0))
                                .border_1()
                                .border_color(panel_border)
                                .bg(panel_bg)
                                .shadow_lg()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .on_action(cx.listener(
                                    |this, _: &gpui_component::input::Enter, window, cx| {
                                        this.accept_history_search(window, cx);
                                    },
                                ))
                                .on_key_down(cx.listener(
                                    |this, event: &KeyDownEvent, window, cx| {
                                        match event.keystroke.key.as_str() {
                                            "down" => {
                                                cx.stop_propagation();
                                                this.move_history_search_selection(1, cx);
                                            }
                                            "up" => {
                                                cx.stop_propagation();
                                                this.move_history_search_selection(-1, cx);
                                            }
                                            "escape" => {
                                                cx.stop_propagation();
                                                this.close_history_search(window, cx);
                                            }
                                            _ => {}
                                        }
                                    },
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .font_weight(gpui::FontWeight::BOLD)
                                                .text_color(tokens.text_muted)
                                                .child("SEARCH COMMAND HISTORY"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(tokens.text_muted)
                                                .child("Esc to cancel"),
                                        ),
                                )
                                .child(
                                    div().font_family(MONO_FONT).text_size(px(13.0)).child(
                                        Input::new(&search.input)
                                            .appearance(true)
                                            .bordered(true)
                                            .px_2()
                                            .py_1(),
                                    ),
                                )
                                .child(
                                    div()
                                        .id("history-search-results")
                                        .max_h(px(300.0))
                                        .overflow_y_scrollbar()
                                        .flex()
                                        .flex_col()
                                        .gap(px(1.0))
                                        .children(rows),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(tokens.text_muted)
                                        .child("↑↓ navigate · Enter insert · Esc cancel"),
                                ),
                        ),
                ),
        )
        .priority(210)
        .into_any_element()
    }

    #[allow(dead_code)]
    fn toggle_plus_menu(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        pane.plus_menu_open = !pane.plus_menu_open;
        cx.notify();
    }

    fn run_quick_action(
        &mut self,
        pane_id: usize,
        cmd: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        pane.plus_menu_open = false;
        pane.terminal.update(cx, |term, cx| {
            term.send_text(&format!("{cmd}\r"), cx);
        });
        if pane.command_history.last().map(|s| s.as_str()) != Some(&cmd) {
            pane.command_history.push(cmd.to_string());
        }
        if let Some(input_state) = &pane.input_state {
            input_state.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn render_quick_action_item(
        label: &'static str,
        cmd: &'static str,
        icon: &'static str,
        pane_id: usize,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("quick-action-{pane_id}-{cmd}")))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(4.0))
            .cursor(CursorStyle::PointingHand)
            .hover(|s| s.bg(tokens.pane_header_btn_hover))
            .child(
                svg()
                    .path(icon)
                    .size(px(13.0))
                    .text_color(tokens.text_muted),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(tokens.text_primary)
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .font_family(MONO_FONT)
                    .text_color(tokens.text_muted)
                    .ml_auto()
                    .child(cmd),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.run_quick_action(pane_id, cmd, window, cx);
                }),
            )
    }

    fn pane_cwd_path(&self, pane: &TerminalPane) -> Option<std::path::PathBuf> {
        #[cfg(target_os = "linux")]
        if let Some(link) = pane
            .pid
            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
        {
            return Some(link);
        }
        if let Some(cwd) = pane.cwd.clone().filter(|path| path.is_dir()) {
            return Some(cwd);
        }
        if let Some(project) = self
            .pane_index
            .get(&pane.id)
            .and_then(|&(p_idx, _, _)| self.projects.get(p_idx))
        {
            return Some(project.path.clone());
        }
        directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
    }

    fn pane_git_branch(&self, pane: &TerminalPane) -> Option<String> {
        let dir = self.pane_cwd_path(pane)?;
        Self::detect_git_branch(&dir)
    }

    fn detect_git_branch(dir: &std::path::Path) -> Option<String> {
        let mut current = dir;
        loop {
            let git_path = current.join(".git");
            if git_path.is_dir() {
                let head_file = git_path.join("HEAD");
                if let Ok(content) = std::fs::read_to_string(head_file) {
                    let trimmed = content.trim();
                    if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                        return Some(branch.to_string());
                    } else if !trimmed.is_empty() {
                        let short = if trimmed.len() > 7 {
                            &trimmed[..7]
                        } else {
                            trimmed
                        };
                        return Some(short.to_string());
                    }
                }
                return None;
            } else if git_path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&git_path) {
                    for line in content.lines() {
                        if let Some(rest) = line.strip_prefix("gitdir:") {
                            let rel = rest.trim();
                            let target = if std::path::Path::new(rel).is_absolute() {
                                std::path::PathBuf::from(rel)
                            } else {
                                current.join(rel)
                            };
                            let head_file = target.join("HEAD");
                            if let Ok(head_content) = std::fs::read_to_string(head_file) {
                                let trimmed = head_content.trim();
                                if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                                    return Some(branch.to_string());
                                } else if !trimmed.is_empty() {
                                    let short = if trimmed.len() > 7 {
                                        &trimmed[..7]
                                    } else {
                                        trimmed
                                    };
                                    return Some(short.to_string());
                                }
                            }
                        }
                    }
                }
                return None;
            }
            match current.parent() {
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }
        None
    }

    fn pane_cwd_display(&self, pane: &TerminalPane) -> String {
        if let Some(path) = self.pane_cwd_path(pane) {
            Self::format_path_display(&path)
        } else {
            "~".to_string()
        }
    }

    fn format_path_display(path: &std::path::Path) -> String {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
        if let Some(rel) = home.as_ref().and_then(|h| path.strip_prefix(h).ok()) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            } else {
                return format!("~/{}", rel.display());
            }
        }
        path.display().to_string()
    }

    /// Thumbnails of images staged for the next prompt, shown between the
    /// badges row and the text editor.
    fn render_attachment_strip(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if pane.attachments.is_empty() {
            return None;
        }

        let pane_id = pane.id;
        let border: Hsla = if tokens.is_dark {
            rgba(0x34353cff).into()
        } else {
            rgba(0x0000001f).into()
        };
        let hover_border: Hsla = tokens.ring;

        let thumbnails = pane
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                let image_path = attachment.path.clone();
                let attachment = attachment.clone();

                div()
                    .id(SharedString::from(format!(
                        "attachment-thumb-{pane_id}-{index}"
                    )))
                    .relative()
                    .size(px(52.0))
                    .rounded(px(7.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(border)
                    .bg(rgba(0x00000022))
                    .cursor(CursorStyle::PointingHand)
                    .hover(move |style| style.border_color(hover_border))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_image_viewer(&attachment, cx);
                        }),
                    )
                    .child(img(image_path).size_full().object_fit(ObjectFit::Cover))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "attachment-remove-{pane_id}-{index}"
                            )))
                            .absolute()
                            .top(px(2.0))
                            .right(px(2.0))
                            .size(px(15.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgba(0x000000b8))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|style| style.bg(rgba(0x000000e0)))
                            .child(
                                svg()
                                    .path("icons/x.svg")
                                    .size(px(9.0))
                                    .text_color(gpui::white()),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.remove_attachment(pane_id, index, cx);
                                }),
                            ),
                    )
                    .into_any_element()
            });

        Some(
            div()
                .id(("attachment-strip", pane_id))
                .w_full()
                .mt(px(10.0))
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .children(thumbnails)
                .into_any_element(),
        )
    }

    /// Full-window lightbox opened by clicking a thumbnail.
    fn render_image_viewer_layer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(viewer) = self.image_viewer.as_ref() else {
            return div().into_any_element();
        };
        let tokens = self.tokens(cx);
        let path = viewer.path.clone();
        let name = viewer.name.clone();
        let subtitle = viewer
            .dimensions
            .map(|(width, height)| format!("{width} × {height}"))
            .unwrap_or_default();

        let panel_bg: Hsla = if tokens.is_dark {
            rgba(0x101114ff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let panel_border: Hsla = if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };
        let image_bg: Hsla = if tokens.is_dark {
            rgba(0x00000066).into()
        } else {
            rgba(0x00000010).into()
        };

        deferred(
            div()
                .id("image-viewer-layer")
                .size_full()
                .absolute()
                .top_0()
                .left_0()
                .child(
                    div()
                        .id("image-viewer-backdrop")
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0()
                        .bg(rgba(0x000000b8))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.image_viewer = None;
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(52.0))
                        .child(
                            div()
                                .id("image-viewer-panel")
                                .relative()
                                .occlude()
                                .w_full()
                                .h_full()
                                .max_w(px(1100.0))
                                .max_h(px(860.0))
                                .rounded(px(12.0))
                                .border_1()
                                .border_color(panel_border)
                                .bg(panel_bg)
                                .shadow_lg()
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap(px(12.0))
                                        .flex_shrink_0()
                                        .h(px(46.0))
                                        .px(px(14.0))
                                        .border_b_1()
                                        .border_color(panel_border)
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap(px(1.0))
                                                .min_w(px(0.0))
                                                .child(
                                                    div()
                                                        .text_size(px(12.5))
                                                        .font_weight(gpui::FontWeight::BOLD)
                                                        .text_color(tokens.text_primary)
                                                        .child(name),
                                                )
                                                .when(!subtitle.is_empty(), |this| {
                                                    this.child(
                                                        div()
                                                            .text_size(px(10.0))
                                                            .text_color(tokens.text_muted)
                                                            .child(subtitle),
                                                    )
                                                }),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(10.0))
                                                .child(
                                                    div()
                                                        .text_size(px(10.0))
                                                        .text_color(tokens.text_muted)
                                                        .child("Esc or click outside to close"),
                                                )
                                                .child(
                                                    div()
                                                        .id("image-viewer-close")
                                                        .size(px(26.0))
                                                        .rounded(px(6.0))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .cursor(CursorStyle::PointingHand)
                                                        .hover(|style| {
                                                            style.bg(tokens.pane_header_btn_hover)
                                                        })
                                                        .child(
                                                            svg()
                                                                .path("icons/x.svg")
                                                                .size(px(12.0))
                                                                .text_color(tokens.text_muted),
                                                        )
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            cx.listener(|this, _, _, cx| {
                                                                cx.stop_propagation();
                                                                this.image_viewer = None;
                                                                cx.notify();
                                                            }),
                                                        ),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_h(px(0.0))
                                        .bg(image_bg)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            img(path).size_full().object_fit(ObjectFit::Contain),
                                        ),
                                ),
                        ),
                ),
        )
        .into_any_element()
    }

    fn render_kathamo_input_bar(
        &self,
        pane: &TerminalPane,
        _is_focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let pane_id = pane.id;
        let tokens = self.tokens(cx);
        let agent_cwd = pane.chat.active.then_some(pane.chat.cwd.as_ref()).flatten();
        let cwd_display = agent_cwd
            .map(|path| Self::format_path_display(path))
            .unwrap_or_else(|| self.pane_cwd_display(pane));
        let git_branch = if let Some(cwd) = agent_cwd {
            Self::detect_git_branch(cwd)
        } else {
            self.pane_git_branch(pane)
        };
        let model_pill = match pane.chat.selected_model.as_ref() {
            Some(model) if self.effective_default_agent_model() == Some(model) => {
                format!("{} · default", agent_model_label(model))
            }
            Some(model) => agent_model_label(model),
            None => "Automatic (rpi)".to_string(),
        };

        let bar_border: Hsla = if tokens.is_dark {
            rgba(0x1d1d1dff).into()
        } else {
            tokens.pane_border
        };
        let bar_bg: Hsla = if tokens.is_dark {
            rgba(0x050505ff).into()
        } else {
            tokens.pane_header_bg
        };
        let badge_border: Hsla = if tokens.is_dark {
            rgba(0x282828ff).into()
        } else {
            rgba(0x00000018).into()
        };
        let badge_bg: Hsla = if tokens.is_dark {
            rgba(0x111111ff).into()
        } else {
            rgba(0x0000000a).into()
        };
        let mode_active_bg: Hsla = if tokens.is_dark {
            rgba(0x1e1f22ff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let mode_active_icon: Hsla = if tokens.is_dark {
            rgba(0xe5e6fcff).into()
        } else {
            tokens.text_primary
        };
        let terminal_mode_active = !pane.chat.active;
        let agent_mode_active = pane.chat.active;
        let dir_icon_color: Hsla = if tokens.is_dark {
            rgba(0xb2b2c3ff).into()
        } else {
            tokens.text_secondary
        };
        let dir_text_color: Hsla = if tokens.is_dark {
            rgba(0xe5e6fcff).into()
        } else {
            tokens.text_primary
        };
        let git_icon_color: Hsla = if tokens.is_dark {
            rgba(0xadc993ff).into()
        } else {
            rgba(0x2d7a2dff).into()
        };
        let git_text_color: Hsla = if tokens.is_dark {
            rgba(0xd9fcb8ff).into()
        } else {
            rgba(0x236823ff).into()
        };
        let input_element = if let Some(input_state) = &pane.input_state {
            div()
                .id(("kathamo-input-wrapper", pane_id))
                .w_full()
                .min_h(px(24.0))
                .max_h(px(220.0))
                .overflow_y_scrollbar()
                .flex()
                .items_start()
                .font_family(MONO_FONT)
                .text_size(px(14.0))
                .key_context(KATHAMO_INPUT_CONTEXT)
                .on_action(cx.listener(move |this, _: &CompleteCommand, window, cx| {
                    cx.stop_propagation();
                    if !this.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
                        this.start_completion(pane_id, window, cx);
                    }
                }))
                .on_action(cx.listener(move |this, _: &gpui_component::input::Escape, window, cx| {
                    if this.pane_ref(pane_id).is_some_and(|p| p.chat.auth.is_some()) {
                        cx.stop_propagation();
                        this.close_agent_auth(pane_id, window, cx);
                    }
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    this.handle_pane_input_key(pane_id, event, window, cx);
                }))
                .child(
                    div().flex_1().flex().items_center().child(
                        Input::new(input_state)
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .cleanable(false)
                            .px_0()
                            .py_0(),
                    ),
                )
                .into_any_element()
        } else {
            div()
                .id(("kathamo-input-wrapper-empty", pane_id))
                .w_full()
                .min_h(px(24.0))
                .into_any_element()
        };

        let completion_menu = self.render_completion_menu(pane, &tokens, cx);

        let plus_menu = if pane.plus_menu_open {
            Some(
                div()
                    .id(("kathamo-plus-menu", pane_id))
                    .absolute()
                    .bottom(px(100.0))
                    .left(px(12.0))
                    .w(px(220.0))
                    .p(px(6.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(if tokens.is_dark {
                        rgba(0x1e1f22ff)
                    } else {
                        rgba(0xffffffff)
                    })
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(tokens.text_muted)
                            .child("QUICK ACTIONS"),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(3.0))
                            .border_b_1()
                            .border_color(tokens.card_border)
                            .text_size(px(11.0))
                            .text_color(tokens.text_secondary)
                            .child(format!("📁 {}", cwd_display)),
                    )
                    .child(Self::render_quick_action_item(
                        "Clear Terminal",
                        "clear",
                        "icons/x.svg",
                        pane_id,
                        &tokens,
                        cx,
                    ))
                    .child(Self::render_quick_action_item(
                        "List Files",
                        "ls -la",
                        "icons/folder.svg",
                        pane_id,
                        &tokens,
                        cx,
                    ))
                    .child(Self::render_quick_action_item(
                        "Git Status",
                        "git status",
                        "icons/status-check.svg",
                        pane_id,
                        &tokens,
                        cx,
                    ))
                    .child(Self::render_quick_action_item(
                        "Working Directory",
                        "pwd",
                        "icons/monitor.svg",
                        pane_id,
                        &tokens,
                        cx,
                    )),
            )
        } else {
            None
        };

        let attachment_strip = self.render_attachment_strip(pane, &tokens, cx);
        let drop_hover_border: Hsla = tokens.ring;
        let drop_hover_bg: Hsla = if tokens.is_dark {
            rgba(0x11251cff).into()
        } else {
            rgba(0x2d7a2d12).into()
        };

        div()
            .id(("kathamo-input-bar", pane_id))
            .relative()
            .w_full()
            .flex()
            .flex_col()
            .px(px(12.0))
            .pt(px(11.0))
            .pb(px(10.0))
            .border_t_1()
            .border_color(bar_border)
            .bg(bar_bg)
            .can_drop(|value, _, _| {
                value.downcast_ref::<ExternalPaths>().is_some_and(|paths| {
                    paths
                        .paths()
                        .iter()
                        .any(|path| attachments::looks_like_image(path))
                })
            })
            .on_drop::<ExternalPaths>(cx.listener(
                move |this, paths: &ExternalPaths, window, cx| {
                    this.drop_external_images(paths, window, cx);
                },
            ))
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style.bg(drop_hover_bg).border_color(drop_hover_border)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.pane_ref(pane_id).is_some_and(|p| p.chat.auth.is_some()) {
                        return;
                    }
                    this.focus_pane_input(pane_id, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_terminal_context_menu(pane_id, event.position, window, cx);
                }),
            )
            .children(plus_menu)
            .children(completion_menu)
            // Row 1: Badges
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .p(px(2.0))
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(badge_border)
                            .bg(badge_bg)
                            .child(
                                div()
                                    .id(("input-mode-terminal", pane_id))
                                    .debug_selector(|| "input-mode-terminal-pill".to_string())
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .h(px(24.0))
                                    .px(px(8.0))
                                    .rounded(px(5.0))
                                    .bg(if !pane.chat.active {
                                        mode_active_bg
                                    } else {
                                        badge_bg
                                    })
                                    .text_size(px(12.0))
                                    .text_color(if !pane.chat.active {
                                        tokens.text_primary
                                    } else {
                                        tokens.text_muted
                                    })
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(move |style| {
                                        if terminal_mode_active {
                                            style
                                        } else {
                                            style.bg(tokens.pane_header_btn_hover)
                                        }
                                    })
                                    .child(
                                        svg()
                                            .path("icons/terminal-bottom.svg")
                                            .size(px(13.0))
                                            .text_color(if !pane.chat.active {
                                                mode_active_icon
                                            } else {
                                                tokens.text_muted
                                            }),
                                    )
                                    .when(terminal_mode_active, |pill| {
                                        pill.child(div().child("Terminal"))
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.set_chat_mode(pane_id, false, window, cx);
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("input-mode-agent", pane_id))
                                    .debug_selector(|| "input-mode-agent-pill".to_string())
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .h(px(24.0))
                                    .px(px(8.0))
                                    .rounded(px(5.0))
                                    .bg(if pane.chat.active {
                                        mode_active_bg
                                    } else {
                                        badge_bg
                                    })
                                    .text_size(px(12.0))
                                    .text_color(if pane.chat.active {
                                        tokens.text_primary
                                    } else {
                                        tokens.text_muted
                                    })
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(move |style| {
                                        if agent_mode_active {
                                            style
                                        } else {
                                            style.bg(tokens.pane_header_btn_hover)
                                        }
                                    })
                                    .child(
                                        svg()
                                            .path("icons/sparkle.svg")
                                            .size(px(13.0))
                                            .text_color(if pane.chat.active {
                                                mode_active_icon
                                            } else {
                                                tokens.text_muted
                                            }),
                                    )
                                    .when(agent_mode_active, |pill| {
                                        pill.child(div().child("Agent"))
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.set_chat_mode(pane_id, true, window, cx);
                                        }),
                                    ),
                            ),
                    )
                    .when(pane.chat.active, |row| row.child(
                        div().id(("select-agent-model", pane_id))
                            .px(px(8.0)).py(px(3.0)).rounded(px(5.0))
                            .text_size(px(11.0)).text_color(tokens.text_secondary)
                            .cursor(CursorStyle::PointingHand)
                            .hover(|view| view.bg(tokens.pane_header_btn_hover))
                            .child(model_pill)
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_agent_auth(pane_id, AgentDialogPage::Models, window, cx);
                            }))
                    ))
                    .when(pane.chat.active, |row| row.child(
                        div().id(("agent-speed-toggle", pane_id))
                            .px(px(8.0)).py(px(3.0)).rounded(px(5.0))
                            .text_size(px(11.0))
                            .text_color(if pane.chat.fast_mode { tokens.text_primary } else { tokens.text_muted })
                            .bg(if pane.chat.fast_mode { badge_bg } else { bar_bg })
                            .border_1()
                            .border_color(if pane.chat.fast_mode { badge_border } else { tokens.border_subtle })
                            .cursor(CursorStyle::PointingHand)
                            .hover(|view| view.bg(tokens.pane_header_btn_hover))
                            .child(if pane.chat.fast_mode { "⚡ Fast" } else { "🧠 Deep" })
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_fast_mode(pane_id, cx);
                            }))
                    ))
                    .when(pane.chat.active && pane.chat.visible_running(), |row| row.child(
                        div().id(("agent-stop", pane_id))
                            .px(px(8.0)).py(px(3.0)).rounded(px(5.0))
                            .text_size(px(12.0)).text_color(tokens.text_secondary)
                            .cursor(CursorStyle::PointingHand).child("Stop")
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.handle_pane_ctrl_c(pane_id, window, cx);
                            })),
                    ))
                    // Badge 1: Directory
                    .child(
                        div()
                            .id(("kathamo-badge-cwd", pane_id))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .h(px(22.0))
                            .px(px(7.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(badge_border)
                            .bg(badge_bg)
                            .child(
                                svg()
                                    .path("icons/folder.svg")
                                    .size(px(12.0))
                                    .text_color(dir_icon_color),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_size(px(12.0))
                                    .text_color(dir_text_color)
                                    .child(cwd_display),
                            ),
                    )
                    // Badge 2: Git branch
                    .when_some(git_branch, |this, branch| {
                        this.child(
                            div()
                                .id(("kathamo-badge-git", pane_id))
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .h(px(22.0))
                                .px(px(7.0))
                                .rounded(px(5.0))
                                .border_1()
                                .border_color(badge_border)
                                .bg(badge_bg)
                                .child(
                                    svg()
                                        .path("icons/git-branch.svg")
                                        .size(px(12.0))
                                        .text_color(git_icon_color),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.0))
                                        .text_color(git_text_color)
                                        .child(branch),
                                ),
                        )
                    }),
            )
            // Row 2: Staged image attachments
            .children(attachment_strip)
            // Row 3: Input Row
            .child(
                div()
                    .w_full()
                    .mt(px(12.0))
                    .child(input_element),
            )
            .child(
                div()
                    .w_full()
                    .mt(px(7.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(10.0))
                    .text_color(tokens.text_muted)
                    .child(div().child(
                        if pane.chat.active {
                            "Enter send · Shift+Enter newline · Ctrl+C stop · Drop or paste images"
                        } else {
                            "Tab complete · ↑↓ history · Ctrl+R search · Shift+Enter newline · Drop or paste images"
                        },
                    )),
            )
    }

    /// Kathamo-style completion popup, anchored above the input bar.
    fn render_completion_menu(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let pane_id = pane.id;
        let menu = pane.completion_menu.as_ref()?;
        if menu.items.is_empty() {
            return None;
        }

        let bg: Hsla = if tokens.is_dark {
            rgba(0x16171eff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let border: Hsla = if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };
        let row_selected_bg: Hsla = if tokens.is_dark {
            rgba(0x262838ff).into()
        } else {
            rgba(0x0000000f).into()
        };
        let row_hover_bg: Hsla = if tokens.is_dark {
            rgba(0x1f2130ff).into()
        } else {
            rgba(0x0000000a).into()
        };

        let rows: Vec<gpui::AnyElement> = menu
            .items
            .iter()
            .enumerate()
            .take(9)
            .map(|(index, item)| {
                let selected = index == menu.selected;
                let kind_color: Hsla = match item.kind {
                    completion::CompletionKind::Directory => rgba(0x54aeffff).into(),
                    completion::CompletionKind::File => tokens.text_secondary,
                    completion::CompletionKind::Branch => rgba(0x9ecb7bff).into(),
                    completion::CompletionKind::History => rgba(0xd7ba7dff).into(),
                    completion::CompletionKind::Command => rgba(0xc5a3ffff).into(),
                };
                div()
                    .id(("completion-item", pane_id * 1000 + index))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .cursor(CursorStyle::PointingHand)
                    .when(selected, |this| this.bg(row_selected_bg))
                    .hover(|this| this.bg(row_hover_bg))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .min_w_0()
                            .child(
                                div()
                                    .w(px(14.0))
                                    .text_size(px(10.0))
                                    .text_color(kind_color)
                                    .child(if item.is_dir { "▸" } else { "·" }),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_size(px(12.5))
                                    .text_color(tokens.text_primary)
                                    .truncate()
                                    .child(item.display.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(tokens.text_muted)
                            .child(item.detail.clone()),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.accept_completion_at(pane_id, index, window, cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .id(("completion-menu", pane_id))
                .absolute()
                .bottom(relative(1.0))
                .left(px(0.0))
                .mb(px(6.0))
                .w(px(440.0))
                .max_w(relative(1.0))
                .max_h(px(240.0))
                .overflow_y_scrollbar()
                .p(px(5.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(border)
                .bg(bg)
                .shadow_lg()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_agent_auth(
        &self,
        pane_id: usize,
        auth: &AuthModal,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tokens = self.tokens(cx);
        let current_model = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.chat.selected_model.clone());

        let mut providers: Vec<&agent_auth::Provider> = auth.providers.iter().collect();
        providers.sort_by_key(|provider| (!provider.ready, provider.id.clone()));
        let provider_rows = providers
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                let id = provider.id.clone();
                let ready = provider.ready;
                let selected = auth.selected.as_deref() == Some(id.as_str());
                let source = provider.source.clone().unwrap_or_else(|| {
                    if ready {
                        "Connected".to_string()
                    } else {
                        "Not connected".to_string()
                    }
                });
                div()
                    .id(("auth-provider", index))
                    .debug_selector(move || format!("auth-provider-{index}"))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(6.0))
                    .text_color(tokens.text_primary)
                    .when(selected, |view| view.bg(tokens.pane_header_bg))
                    .hover(|view| view.bg(tokens.pane_header_btn_hover))
                    .cursor(CursorStyle::PointingHand)
                    .child(
                        div()
                            .size(px(7.0))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(if ready {
                                tokens.block_success
                            } else {
                                tokens.text_muted
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(id.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.5))
                                    .text_color(tokens.text_muted)
                                    .child(source),
                            ),
                    )
                    .when(ready, |view| {
                        let disconnect_id = id.clone();
                        view.child(
                            div()
                                .id(("auth-disconnect", index))
                                .px(px(7.0))
                                .py(px(2.0))
                                .rounded(px(4.0))
                                .text_size(px(10.5))
                                .text_color(tokens.block_failure)
                                .cursor(CursorStyle::PointingHand)
                                .hover(|view| view.opacity(0.8))
                                .child("Disconnect")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.disconnect_agent_provider(
                                            pane_id,
                                            disconnect_id.clone(),
                                            window,
                                            cx,
                                        );
                                    }),
                                ),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if ready {
                                tokens.text_secondary
                            } else {
                                tokens.text_primary
                            })
                            .child(if ready { "Use models" } else { "Connect" }),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if ready {
                                this.set_agent_dialog_page(
                                    pane_id,
                                    AgentDialogPage::Models,
                                    window,
                                    cx,
                                );
                            } else {
                                this.select_provider_for_auth(pane_id, id.clone(), window, cx);
                            }
                        }),
                    )
            })
            .collect::<Vec<_>>();
        let connected_models: Vec<&agent_auth::AgentModel> = auth
            .models
            .iter()
            .filter(|model| {
                auth.providers
                    .iter()
                    .any(|provider| provider.id == model.provider && provider.ready)
            })
            .collect();
        let has_connected_models = !connected_models.is_empty();
        let mut model_rows: Vec<AnyElement> = Vec::new();
        let mut last_provider: Option<&str> = None;
        for (index, model) in connected_models.iter().enumerate() {
            if last_provider != Some(model.provider.as_str()) {
                last_provider = Some(model.provider.as_str());
                model_rows.push(
                    div()
                        .id(("agent-model-provider", index))
                        .mt(px(4.0))
                        .px(px(10.0))
                        .pb(px(2.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(tokens.text_muted)
                                .child(model.provider.to_uppercase()),
                        )
                        .child(
                            div()
                                .text_size(px(9.5))
                                .text_color(tokens.text_muted)
                                .child(match model.backend {
                                    agent_auth::ModelBackend::Rpi => "rpi",
                                    agent_auth::ModelBackend::Omp => "OMP",
                                }),
                        )
                        .into_any_element(),
                );
            }
            let is_selected = current_model.as_ref() == Some(*model);
            let is_default = self.effective_default_agent_model() == Some(*model);
            let selected_model = (*model).clone();
            let default_model = (*model).clone();
            model_rows.push(
                div()
                    .id(("agent-model", index))
                    .debug_selector(move || format!("agent-model-{index}"))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .text_color(tokens.text_primary)
                    .cursor(CursorStyle::PointingHand)
                    .when(is_selected, |view| view.bg(tokens.pane_header_bg))
                    .hover(|view| view.bg(tokens.pane_header_btn_hover))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .min_w_0()
                            .child(div().w(px(12.0)).flex_shrink_0().when(is_selected, |slot| {
                                slot.child(
                                    svg()
                                        .path("icons/status-check.svg")
                                        .size(px(11.0))
                                        .text_color(tokens.block_success),
                                )
                            }))
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_size(px(12.0))
                                    .truncate()
                                    .child(model.id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .id(("default-agent-model", index))
                            .text_size(px(10.5))
                            .cursor(CursorStyle::PointingHand)
                            .text_color(if is_default {
                                tokens.text_muted
                            } else {
                                tokens.text_secondary
                            })
                            .child(if is_default { "Default" } else { "Set default" })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.select_agent_model(
                                        pane_id,
                                        Some(default_model.clone()),
                                        true,
                                        window,
                                        cx,
                                    );
                                }),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.select_agent_model(
                                pane_id,
                                Some(selected_model.clone()),
                                false,
                                window,
                                cx,
                            );
                        }),
                    )
                    .into_any_element(),
            );
        }
        deferred(
            div().id(("auth-layer", pane_id)).absolute().size_full().top_0().left_0()
                .flex().items_center().justify_center()
                .bg(rgba(0x00000088))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_agent_auth(pane_id, window, cx);
                }))
                .child(
                    div().id(("auth-panel", pane_id))
                        .track_focus(&auth.focus)
                        .w(px(520.0)).max_w(relative(0.95)).max_h(relative(0.9))
                        .p(px(16.0)).rounded(px(10.0)).border_1()
                        .border_color(tokens.pane_border).bg(tokens.pane_bg).shadow_lg()
                        .flex().flex_col().gap(px(10.0))
                        .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                cx.stop_propagation();
                                this.close_agent_auth(pane_id, window, cx);
                            }
                        }))
                        .on_action(cx.listener(move |this, _: &gpui_component::input::Enter, window, cx| {
                            cx.stop_propagation();
                            this.submit_agent_auth(pane_id, window, cx);
                        }))
                        .child(div().flex().justify_between().items_center()
                            .child(div().text_size(px(15.0)).font_weight(gpui::FontWeight::BOLD)
                                .text_color(tokens.text_primary).child(if auth.page == AgentDialogPage::Models { "Choose model" } else { "Authenticate provider" }))
                            .child(div().id(("auth-close", pane_id)).cursor(CursorStyle::PointingHand)
                                .text_color(tokens.text_muted).child("Close")
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_agent_auth(pane_id, window, cx);
                                }))))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(2.0))
                                .p(px(2.0))
                                .rounded(px(8.0))
                                .bg(tokens.card_bg)
                                .child(
                                    div()
                                        .id(("auth-tab", pane_id))
                                        .debug_selector(|| "agent-auth-tab".into())
                                        .px(px(10.0))
                                        .py(px(4.0))
                                        .rounded(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(if auth.page == AgentDialogPage::Auth {
                                            tokens.text_primary
                                        } else {
                                            tokens.text_muted
                                        })
                                        .when(auth.page == AgentDialogPage::Auth, |tab| {
                                            tab.bg(tokens.pane_bg)
                                        })
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|tab| tab.text_color(tokens.text_primary))
                                        .child("Providers")
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.set_agent_dialog_page(
                                                    pane_id,
                                                    AgentDialogPage::Auth,
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id(("models-tab", pane_id))
                                        .px(px(10.0))
                                        .py(px(4.0))
                                        .rounded(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(if auth.page == AgentDialogPage::Models {
                                            tokens.text_primary
                                        } else {
                                            tokens.text_muted
                                        })
                                        .when(auth.page == AgentDialogPage::Models, |tab| {
                                            tab.bg(tokens.pane_bg)
                                        })
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|tab| tab.text_color(tokens.text_primary))
                                        .child("Models")
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.set_agent_dialog_page(
                                                    pane_id,
                                                    AgentDialogPage::Models,
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                ),
                        )
                        .when(auth.page == AgentDialogPage::Auth, |view| {
                            view.child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(tokens.text_muted)
                                    .child("Sign in to a provider to unlock its models."),
                            )
                            .when(auth.discovery.is_some() && auth.providers.is_empty(), |view| {
                                view.child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(tokens.text_muted)
                                        .child("Checking providers…"),
                                )
                            })
                            .child(
                                div()
                                    .id(("auth-providers", pane_id))
                                    .max_h(px(220.0))
                                    .overflow_y_scrollbar()
                                    .flex()
                                    .flex_col()
                                    .gap(px(1.0))
                                    .children(provider_rows),
                            )
                            .when(
                                auth.discovery.is_none() && auth.providers.is_empty(),
                                |view| {
                                    view.child(
                                        div()
                                            .id(("auth-retry", pane_id))
                                            .text_size(px(11.5))
                                            .text_color(tokens.text_secondary)
                                            .cursor(CursorStyle::PointingHand)
                                            .child("Retry provider discovery")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    if let Some(auth) = this
                                                        .pane_mut(pane_id)
                                                        .and_then(|pane| pane.chat.auth.as_mut())
                                                    {
                                                        auth.error = None;
                                                        auth.discover();
                                                        cx.notify();
                                                    }
                                                }),
                                            ),
                                    )
                                },
                            )
                            .when_some(auth.error.as_ref(), |view, error| {
                                view.child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(tokens.block_failure)
                                        .child(error.clone()),
                                )
                            })
                            .when_some(auth.selected.as_ref(), |view, selected| {
                                let spec = crate::provider::get_provider(selected);
                                let provider_name = spec.map(|s| s.name.as_str()).unwrap_or(selected.as_str());
                                let supports_oauth = spec.map(|s| s.supports_oauth()).unwrap_or(false);
                                let supports_api_key = spec.map(|s| s.supports_api_key()).unwrap_or(true);
                                let primary_var = spec.and_then(|s| s.primary_env_var()).unwrap_or("API_KEY");
                                let is_connected = auth.providers.iter().any(|p| p.id == *selected && p.ready);

                                view.child(
                                    div()
                                        .mt(px(8.0))
                                        .p(px(10.0))
                                        .rounded(px(6.0))
                                        .bg(tokens.pane_header_bg)
                                        .border_1()
                                        .border_color(tokens.card_border)
                                        .flex()
                                        .flex_col()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap(px(8.0))
                                                        .child(
                                                            div()
                                                                .text_size(px(12.5))
                                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                                .text_color(tokens.text_primary)
                                                                .child(format!("Configure {provider_name}")),
                                                        )
                                                        .when(is_connected, |header| {
                                                            header.child(
                                                                div()
                                                                    .px(px(6.0))
                                                                    .py(px(1.5))
                                                                    .rounded(px(4.0))
                                                                    .bg(tokens.card_hover)
                                                                    .text_size(px(10.0))
                                                                    .text_color(tokens.block_success)
                                                                    .child("Connected"),
                                                            )
                                                        }),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap(px(8.0))
                                                        .when(is_connected, |actions| {
                                                            let disconnect_id = selected.clone();
                                                            actions.child(
                                                                div()
                                                                    .id(("card-disconnect-btn", pane_id))
                                                                    .px(px(8.0))
                                                                    .py(px(3.0))
                                                                    .rounded(px(4.0))
                                                                    .bg(tokens.card_hover)
                                                                    .hover(|v| v.bg(tokens.pane_header_btn_hover))
                                                                    .text_color(tokens.block_failure)
                                                                    .text_size(px(11.0))
                                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                                    .cursor(CursorStyle::PointingHand)
                                                                    .child("Disconnect")
                                                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                        cx.stop_propagation();
                                                                        this.disconnect_agent_provider(pane_id, disconnect_id.clone(), window, cx);
                                                                    })),
                                                            )
                                                        })
                                                        .child(
                                                            div()
                                                                .text_size(px(11.0))
                                                                .text_color(tokens.text_muted)
                                                                .cursor(CursorStyle::PointingHand)
                                                                .child("✕")
                                                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                                                    cx.stop_propagation();
                                                                    if let Some(auth) = this.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut()) {
                                                                        if let Some(mut login) = auth.login.take() {
                                                                            login.cancel();
                                                                        }
                                                                        auth.selected = None;
                                                                        auth.output.clear();
                                                                        auth.error = None;
                                                                        cx.notify();
                                                                    }
                                                                })),
                                                        ),
                                                ),
                                        )
                                        .when(supports_oauth && auth.login.is_none(), |panel| {
                                            let login_id = selected.clone();
                                            panel.child(
                                                div()
                                                    .id(("start-oauth-btn", pane_id))
                                                    .px(px(12.0))
                                                    .py(px(6.0))
                                                    .rounded(px(5.0))
                                                    .bg(tokens.primary_btn_bg)
                                                    .text_color(tokens.primary_btn_fg)
                                                    .text_size(px(12.0))
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .cursor(CursorStyle::PointingHand)
                                                    .hover(|v| v.bg(tokens.primary_btn_hover))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(if is_connected { "Re-authenticate with OAuth" } else { "Connect with OAuth (Browser)" })
                                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        this.start_agent_oauth(pane_id, login_id.clone(), window, cx);
                                                    })),
                                            )
                                        })
                                        .when(supports_api_key && auth.login.is_none(), |panel| {
                                            panel.child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap(px(4.0))
                                                    .when(supports_oauth, |section| {
                                                        section.child(
                                                            div()
                                                                .text_size(px(10.5))
                                                                .text_color(tokens.text_muted)
                                                                .child("— OR ENTER API KEY —"),
                                                        )
                                                    })
                                                    .child(
                                                        div()
                                                            .text_size(px(11.0))
                                                            .text_color(tokens.text_muted)
                                                            .child(format!("API Key ({primary_var}):")),
                                                    )
                                                    .child(
                                                        Input::new(&auth.input)
                                                            .appearance(true)
                                                            .bordered(true)
                                                            .cleanable(true),
                                                    )
                                                    .child(
                                                        div()
                                                            .flex()
                                                            .justify_end()
                                                            .child(
                                                                div()
                                                                    .id(("save-api-key-btn", pane_id))
                                                                    .px(px(10.0))
                                                                    .py(px(4.0))
                                                                    .rounded(px(4.0))
                                                                    .bg(tokens.card_hover)
                                                                    .hover(|v| v.bg(tokens.pane_header_btn_hover))
                                                                    .text_color(tokens.text_primary)
                                                                    .text_size(px(11.5))
                                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                                    .cursor(CursorStyle::PointingHand)
                                                                    .child(if is_connected { "Update API Key" } else { "Save Key & Connect" })
                                                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                        cx.stop_propagation();
                                                                        this.submit_agent_auth(pane_id, window, cx);
                                                                    })),
                                                            ),
                                                    ),
                                            )
                                        })
                                        .when(auth.login.is_some(), |panel| {
                                            panel.child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap(px(6.0))
                                                    .when(!auth.output.is_empty(), |out_view| {
                                                        out_view.child(
                                                            div()
                                                                .id(("auth-output", pane_id))
                                                                .max_h(px(120.0))
                                                                .overflow_y_scrollbar()
                                                                .text_size(px(11.0))
                                                                .font_family(MONO_FONT)
                                                                .text_color(tokens.text_primary)
                                                                .child(auth.output.clone()),
                                                        )
                                                    })
                                                    .child(
                                                        Input::new(&auth.input)
                                                            .appearance(true)
                                                            .bordered(true)
                                                            .cleanable(true),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_size(px(10.5))
                                                            .text_color(tokens.text_muted)
                                                            .child("Enter code or response if prompted, then press Enter."),
                                                    ),
                                            )
                                        }),
                                )
                            })
                            .when(auth.selected.is_none(), |view| {
                                view.child(
                                    Input::new(&auth.provider_input)
                                        .appearance(true)
                                        .bordered(true),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child(
                                            "Select a provider above, or enter an extension provider ID and press Enter.",
                                        ),
                                )
                            })
                        })
                        .when(auth.page == AgentDialogPage::Models, |view| {
                            view.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(11.5))
                                            .text_color(tokens.text_muted)
                                            .child(if auth.discovery.is_some() && !has_connected_models {
                                                "Checking provider connections…"
                                            } else if has_connected_models {
                                                "Models from connected providers"
                                            } else {
                                                "No connected providers"
                                            }),
                                    )
                                    .when(auth.model_discovery.is_none(), |header| {
                                        header.child(
                                            div()
                                                .id(("refresh-models", pane_id))
                                                .text_size(px(10.5))
                                                .text_color(tokens.text_muted)
                                                .cursor(CursorStyle::PointingHand)
                                                .hover(|view| view.text_color(tokens.text_primary))
                                                .child("Refresh")
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        if let Some(auth) = this
                                                            .pane_mut(pane_id)
                                                            .and_then(|pane| pane.chat.auth.as_mut())
                                                        {
                                                            auth.discover();
                                                            cx.notify();
                                                        }
                                                    }),
                                                ),
                                        )
                                    }),
                            )
                            .when(auth.model_discovery.is_some(), |view| {
                                view.child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(tokens.text_muted)
                                        .child("Loading models…"),
                                )
                            })
                            .when_some(auth.model_error.as_ref(), |view, error| {
                                view.child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(tokens.block_failure)
                                        .child(error.clone()),
                                )
                            })
                            .when(
                                auth.model_discovery.is_none() && has_connected_models,
                                |view| {
                                    view.child(
                                        div()
                                            .id(("agent-models", pane_id))
                                            .max_h(px(240.0))
                                            .overflow_y_scrollbar()
                                            .flex()
                                            .flex_col()
                                            .gap(px(1.0))
                                            .children(model_rows),
                                    )
                                },
                            )
                            .when(
                                auth.model_discovery.is_none()
                                    && !has_connected_models
                                    && auth.discovery.is_none(),
                                |view| {
                                    view.child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .gap(px(6.0))
                                            .py(px(12.0))
                                            .child(
                                                div()
                                                    .text_size(px(12.0))
                                                    .text_color(tokens.text_secondary)
                                                    .child(
                                                        "No models available from connected providers.",
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .id(("models-open-providers", pane_id))
                                                    .px(px(10.0))
                                                    .py(px(4.0))
                                                    .rounded(px(5.0))
                                                    .bg(tokens.card_hover)
                                                    .text_size(px(11.0))
                                                    .text_color(tokens.text_primary)
                                                    .cursor(CursorStyle::PointingHand)
                                                    .child("Open providers")
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |this, _, window, cx| {
                                                            cx.stop_propagation();
                                                            this.set_agent_dialog_page(
                                                                pane_id,
                                                                AgentDialogPage::Auth,
                                                                window,
                                                                cx,
                                                            );
                                                        }),
                                                    ),
                                            ),
                                    )
                                },
                            )
                            .when_some(auth.error.as_ref(), |view, error| {
                                view.child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(tokens.block_failure)
                                        .child(error.clone()),
                                )
                            })
                            .child(
                                div()
                                    .pt(px(8.0))
                                    .border_t_1()
                                    .border_color(tokens.border_subtle)
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .id(("use-rpi", pane_id))
                                            .text_size(px(10.5))
                                            .text_color(tokens.text_secondary)
                                            .cursor(CursorStyle::PointingHand)
                                            .hover(|view| view.text_color(tokens.text_primary))
                                            .child("Use rpi automatic model")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.select_agent_model(
                                                        pane_id, None, false, window, cx,
                                                    );
                                                }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id(("reset-default-model", pane_id))
                                            .text_size(px(10.5))
                                            .text_color(tokens.text_secondary)
                                            .cursor(CursorStyle::PointingHand)
                                            .hover(|view| view.text_color(tokens.text_primary))
                                            .child("Reset default")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.select_agent_model(
                                                        pane_id, None, true, window, cx,
                                                    );
                                                }),
                                            ),
                                    ),
                            )
                        })
                        .child(if auth.page == AgentDialogPage::Auth {
                            div()
                                .text_size(px(10.5))
                                .text_color(tokens.text_muted)
                                .child("Esc or Close cancels login.")
                        } else {
                            div()
                                .text_size(px(10.5))
                                .text_color(tokens.text_muted)
                                .child("Select a model for this pane; Esc or Close dismisses.")
                        })
                )
        ).priority(210).into_any_element()
    }

    fn render_agent_chat(
        &self,
        pane: &TerminalPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tokens = self.tokens(cx);
        let conversation = format!("{:?}", pane.chat.selected_model);
        let pane_id = pane.id;
        let is_autoscroll = pane.chat.autoscroll;
        let offset_y = pane.chat.scroll.offset().y;
        let max_height = pane.chat.scroll.max_offset().y;
        let dist_from_bottom = max_height + offset_y;

        if is_autoscroll {
            pane.chat.scroll.scroll_to_bottom();
        }

        let messages = pane
            .chat
            .visible_messages()
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let inner: AnyElement = match message.role {
                    ChatRole::User => div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .child(
                            div()
                                .max_w(px(540.0))
                                .min_w_0()
                                .px(px(14.0))
                                .py(px(8.0))
                                .rounded(px(12.0))
                                .bg(tokens.card_hover)
                                .text_size(px(13.5))
                                .line_height(relative(1.85))
                                .child(chat_markdown(
                                    SharedString::from(format!(
                                        "agent-user-{pane_id}-{conversation}-{index}"
                                    )),
                                    message.text.clone(),
                                    px(13.5),
                                    rems(0.5),
                                    tokens,
                                    window,
                                    cx,
                                )),
                        )
                        .into_any_element(),
                    ChatRole::Assistant => {
                        let id = SharedString::from(format!(
                            "agent-markdown-{pane_id}-{conversation}-{index}"
                        ));
                        let mut body = div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .text_size(px(13.0))
                            .line_height(relative(1.9))
                            .text_color(tokens.text_primary);

                        if message.streaming {
                            // Re-parse only settled blocks while text is arriving; the live tail
                            // stays plain so every token does not re-shape the whole message.
                            let (settled, tail) = split_streaming_markdown(&message.text);
                            if !settled.trim().is_empty() {
                                body = body.child(chat_markdown(
                                    id.clone(),
                                    settled.to_string(),
                                    px(13.0),
                                    rems(0.6),
                                    tokens,
                                    window,
                                    cx,
                                ));
                            }
                            if !tail.is_empty() {
                                // The live block gets its own TextView: only this block is
                                // re-parsed per token, while settled blocks stay cached.
                                body = body.child(chat_markdown(
                                    SharedString::from(format!(
                                        "agent-markdown-{pane_id}-{conversation}-{index}-live"
                                    )),
                                    tail.to_string(),
                                    px(13.0),
                                    rems(0.6),
                                    tokens,
                                    window,
                                    cx,
                                ));
                            }
                        } else {
                            body = body.child(chat_markdown(
                                id,
                                message.text.clone(),
                                px(13.0),
                                rems(0.6),
                                tokens,
                                window,
                                cx,
                            ));
                        }

                        body.into_any_element()
                    }
                    ChatRole::Thinking => {
                        let is_expanded = message.expanded;
                        let summary =
                            clip_tool_text(message.text.lines().next().unwrap_or("").trim(), 88).0;
                        let mut panel = div()
                            .w_full()
                            .min_w_0()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(tokens.border_subtle)
                            .bg(tokens.card_hover)
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "thinking-header-{pane_id}-{index}"
                                    )))
                                    .w_full()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .px(px(12.0))
                                    .py(px(8.0))
                                    .cursor(CursorStyle::PointingHand)
                                    .text_size(px(11.5))
                                    .text_color(tokens.text_muted)
                                    .hover(move |style| style.text_color(tokens.text_primary))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.toggle_chat_tool_expanded(pane_id, index, cx);
                                        }),
                                    )
                                    .child(
                                        svg()
                                            .path(if is_expanded {
                                                "icons/chevron-down.svg"
                                            } else {
                                                "icons/chevron-right.svg"
                                            })
                                            .size(px(11.0))
                                            .flex_shrink_0()
                                            .text_color(tokens.text_muted),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(tokens.text_primary)
                                            .child("Thinking"),
                                    )
                                    .when(!is_expanded && !summary.is_empty(), |header| {
                                        header.child(
                                            div()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(summary),
                                        )
                                    }),
                            );
                        if is_expanded {
                            panel = panel.child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .pl(px(30.0))
                                    .pr(px(12.0))
                                    .pb(px(12.0))
                                    .whitespace_normal()
                                    .text_size(px(11.5))
                                    .line_height(relative(1.7))
                                    .text_color(tokens.text_muted)
                                    .child(message.text.clone()),
                            );
                        }
                        panel.into_any_element()
                    }
                    ChatRole::Status => div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .max_w(px(520.0))
                                .truncate()
                                .px(px(10.0))
                                .py(px(3.0))
                                .rounded_full()
                                .bg(tokens.card_hover)
                                .text_size(px(11.0))
                                .text_color(tokens.text_muted)
                                .child(message.text.clone()),
                        )
                        .into_any_element(),
                    ChatRole::Tool => {
                        if let Some(tool) = &message.tool {
                            let on_toggle = cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.toggle_chat_tool_expanded(pane_id, index, cx);
                            });
                            chat_tool_card(pane_id, index, tool, tokens, on_toggle)
                                .into_any_element()
                        } else {
                            div()
                                .w_full()
                                .min_w_0()
                                .px(px(10.0))
                                .py(px(6.0))
                                .rounded(px(5.0))
                                .text_size(px(11.0))
                                .text_color(tokens.text_muted)
                                .child(message.text.clone())
                                .into_any_element()
                        }
                    }
                    ChatRole::Error => div()
                        .w_full()
                        .min_w_0()
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(8.0))
                        .bg(tokens.block_failure.opacity(0.08))
                        .border_1()
                        .border_color(tokens.block_failure.opacity(0.35))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tokens.block_failure)
                                .child("Error"),
                        )
                        .child(
                            div()
                                .w_full()
                                .whitespace_normal()
                                .font_family(MONO_FONT)
                                .text_size(px(11.5))
                                .line_height(relative(1.6))
                                .text_color(tokens.block_failure)
                                .child(message.text.clone()),
                        )
                        .into_any_element(),
                };

                div().w_full().min_w_0().child(inner).into_any_element()
            })
            .collect::<Vec<_>>();
        let default_screen: Option<AnyElement> = if pane.chat.visible_messages().is_empty() {
            let effective = pane.chat.selected_model.as_ref();
            let inherited =
                effective.is_some() && self.effective_default_agent_model() == effective;
            let cwd = pane
                .chat
                .cwd
                .as_ref()
                .or(pane.cwd.as_ref())
                .map(|path| Self::format_path_display(path))
                .unwrap_or_else(|| "this project".to_string());
            let backend = effective.map(|model| match model.backend {
                agent_auth::ModelBackend::Rpi => "rpi",
                agent_auth::ModelBackend::Omp => "OMP",
            });
            let notice = pane.chat.visible_startup_error().map(agent_startup_notice);

            let mut screen = div()
                .id(("agent-default-screen", pane_id))
                .debug_selector(|| "agent-default-screen".to_string())
                .w_full()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.0))
                .py(px(24.0))
                .child(
                    div()
                        .size(px(44.0))
                        .rounded(px(12.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(tokens.card_bg)
                        .child(
                            svg()
                                .path("icons/sparkle.svg")
                                .size(px(22.0))
                                .text_color(tokens.text_secondary),
                        ),
                )
                .child(
                    div()
                        .text_size(px(15.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(tokens.text_primary)
                        .child("Kathamo Agent"),
                )
                .child(
                    div()
                        .max_w(px(460.0))
                        .text_center()
                        .text_size(px(12.0))
                        .line_height(relative(1.6))
                        .text_color(tokens.text_muted)
                        .child(format!(
                            "Working in {cwd}. Ask it to read, edit, or run anything here."
                        )),
                );
            if let Some(model) = effective {
                screen = screen.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded_full()
                        .border_1()
                        .border_color(tokens.border_subtle)
                        .bg(tokens.card_hover)
                        .child(
                            svg()
                                .path("icons/sparkle.svg")
                                .size(px(11.0))
                                .text_color(tokens.text_secondary),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tokens.text_primary)
                                .child(agent_model_label(model)),
                        )
                        .when(inherited, |pill| {
                            pill.child(
                                div()
                                    .px(px(4.0))
                                    .text_size(px(10.0))
                                    .text_color(tokens.text_muted)
                                    .child("default"),
                            )
                        })
                        .when_some(backend, |pill, backend| {
                            pill.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(tokens.text_muted)
                                    .child(backend),
                            )
                        }),
                );
            }
            screen =
                screen.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(3.0))
                        .text_size(px(11.0))
                        .text_color(tokens.text_muted)
                        .child(div().child("Type a prompt below to start"))
                        .child(div().child(
                            "/model chooses a model · /auth signs in · ⚡ Fast switches depth",
                        )),
                );
            if let Some((message, detail)) = notice {
                screen = screen.child(
                    div()
                        .max_w(px(480.0))
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(8.0))
                        .bg(tokens.block_failure.opacity(0.08))
                        .border_1()
                        .border_color(tokens.block_failure.opacity(0.3))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tokens.block_failure)
                                .child(message),
                        )
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_size(px(10.5))
                                .line_height(relative(1.5))
                                .whitespace_normal()
                                .text_color(tokens.text_muted)
                                .child(detail),
                        ),
                );
            }
            Some(screen.into_any_element())
        } else {
            None
        };

        let chat_view = div()
            .id(("agent-chat", pane_id))
            .size_full()
            .track_scroll(&pane.chat.scroll)
            .overflow_y_scroll()
            .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                if let Some(pane) = this.pane_mut(pane_id) {
                    let line_height = window.line_height();
                    let delta = event.delta.pixel_delta(line_height);
                    let offset_y = pane.chat.scroll.offset().y;
                    let max_height = pane.chat.scroll.max_offset().y;
                    let dist = max_height + offset_y;
                    if delta.y > px(0.0) {
                        if pane.chat.autoscroll {
                            pane.chat.autoscroll = false;
                            cx.notify();
                        }
                    } else if delta.y < px(0.0) {
                        if dist <= px(40.0) {
                            if !pane.chat.autoscroll {
                                pane.chat.autoscroll = true;
                                pane.chat.scroll.scroll_to_bottom();
                                cx.notify();
                            }
                        }
                    }
                }
            }))
            .px(px(24.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .when_some(default_screen, |view, screen| view.child(screen))
            .children(messages)
            .when(pane.chat.visible_running(), |view| {
                view.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(4.0))
                        .py(px(2.0))
                        .child(div().size(px(6.0)).rounded_full().bg(tokens.block_warning))
                        .child(
                            div()
                                .text_color(tokens.text_muted)
                                .text_size(px(12.0))
                                .child("Waiting for agent updates…"),
                        ),
                )
            });

        let scroll_to_bottom_btn = if !is_autoscroll && dist_from_bottom > px(32.0) {
            Some(
                div()
                    .id(("scroll-to-bottom-btn", pane_id))
                    .absolute()
                    .bottom(px(16.0))
                    .right(px(24.0))
                    .cursor(CursorStyle::PointingHand)
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded_full()
                    .bg(tokens.pane_header_bg)
                    .border_1()
                    .border_color(tokens.border)
                    .shadow_md()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(tokens.text_primary)
                    .hover(|s| s.bg(tokens.sidebar_item_hover))
                    .child(
                        svg()
                            .path("icons/chevron-down.svg")
                            .size(px(11.0))
                            .text_color(tokens.text_primary),
                    )
                    .child("Scroll to bottom")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(pane) = this.pane_mut(pane_id) {
                                pane.chat.autoscroll = true;
                                pane.chat.scroll.scroll_to_bottom();
                                cx.notify();
                            }
                        }),
                    ),
            )
        } else {
            None
        };

        let scrollbar = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .child(
                gpui_component::scroll::Scrollbar::vertical(&pane.chat.scroll)
                    .id(("agent-chat-scrollbar", pane_id)),
            );

        div()
            .relative()
            .size_full()
            .child(chat_view)
            .child(scrollbar)
            .children(scroll_to_bottom_btn)
    }
    fn render_terminal_pane(
        &self,
        pane: &TerminalPane,
        is_maximized: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let pane_id = pane.id;
        let is_focused = self.focused_pane_id == Some(pane_id);
        let tokens = self.tokens(cx);

        div()
            .id(("terminal-pane", pane_id))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(tokens.pane_bg)
            .rounded(px(8.0))
            .border_1()
            .border_color(if is_focused {
                tokens.pane_border_focused
            } else {
                tokens.pane_border
            })
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.pane_ref(pane_id).is_some_and(|p| p.chat.auth.is_some()) {
                        return;
                    }
                    this.focus_pane(pane_id, window, cx);
                }),
            )
            .child(
                div()
                    .h(px(28.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(8.0))
                    .bg(tokens.pane_header_bg)
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(tokens.pane_header_fg)
                            .child(pane.display_title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .when(!pane.should_show_bottom_input(cx), |row| {
                                row.child(
                                    div()
                                        .id(("header-agent", pane_id))
                                        .px(px(7.0))
                                        .text_size(px(11.0))
                                        .text_color(tokens.pane_header_fg)
                                        .cursor(CursorStyle::PointingHand)
                                        .child("Agent")
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.set_chat_mode(pane_id, true, window, cx);
                                            }),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .id(("split-v-btn", pane_id))
                                    .size(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                    .child(
                                        svg()
                                            .path("icons/split-vertical.svg")
                                            .size(px(13.0))
                                            .text_color(tokens.pane_header_icon)
                                            .hover(|s| s.text_color(tokens.pane_header_icon_hover)),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.split_pane(
                                                pane_id,
                                                gpui::Axis::Horizontal,
                                                window,
                                                cx,
                                            );
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("split-h-btn", pane_id))
                                    .size(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                    .child(
                                        svg()
                                            .path("icons/split-horizontal.svg")
                                            .size(px(13.0))
                                            .text_color(tokens.pane_header_icon)
                                            .hover(|s| s.text_color(tokens.pane_header_icon_hover)),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.split_pane(
                                                pane_id,
                                                gpui::Axis::Vertical,
                                                window,
                                                cx,
                                            );
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("maximize-btn", pane_id))
                                    .size(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                    .child(
                                        svg()
                                            .path(if is_maximized {
                                                "icons/pane-restore.svg"
                                            } else {
                                                "icons/pane-maximize.svg"
                                            })
                                            .size(px(13.0))
                                            .text_color(tokens.pane_header_icon)
                                            .hover(|s| s.text_color(tokens.pane_header_icon_hover)),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.toggle_maximize_pane(pane_id, cx);
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("close-btn", pane_id))
                                    .size(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                    .child(
                                        svg()
                                            .path("icons/pane-close.svg")
                                            .size(px(13.0))
                                            .text_color(tokens.pane_header_icon)
                                            .hover(|s| s.text_color(rgba(0xf87171ff))),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.close_pane(pane_id, window, cx);
                                        }),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.focus_pane(pane_id, window, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_terminal_context_menu(pane_id, event.position, window, cx);
                        }),
                    )
                    .child(if pane.chat.active {
                        self.render_agent_chat(pane, window, cx).into_any_element()
                    } else {
                        pane.terminal.clone().into_any_element()
                    }),
            )
            .when(pane.should_show_bottom_input(cx), |this| {
                this.child(self.render_kathamo_input_bar(pane, is_focused, cx))
            })
            .when_some(pane.chat.auth.as_ref(), |this, auth| {
                this.child(self.render_agent_auth(pane_id, auth, cx))
            })
    }

    fn render_pane_node(
        &self,
        node: &PaneNode,
        session: &Session,
        is_maximized: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match node {
            PaneNode::Leaf(pane_id) => {
                if let Some(pane) = session.panes.iter().find(|p| p.id == *pane_id) {
                    self.render_terminal_pane(pane, is_maximized, window, cx)
                        .into_any_element()
                } else {
                    div().into_any_element()
                }
            }
            PaneNode::Split {
                id,
                axis,
                left,
                right,
            } => {
                let left_el = self.render_pane_node(left, session, is_maximized, window, cx);
                let right_el = self.render_pane_node(right, session, is_maximized, window, cx);
                let is_horizontal = *axis == gpui::Axis::Horizontal;
                let split_id = ("terminal-split", *id);

                if is_horizontal {
                    h_resizable(split_id)
                        .child(
                            resizable_panel().size_range(px(100.0)..px(3000.0)).child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .size_full()
                                    .p(px(2.0))
                                    .child(div().flex_1().size_full().child(left_el)),
                            ),
                        )
                        .child(
                            resizable_panel().size_range(px(100.0)..px(3000.0)).child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .size_full()
                                    .p(px(2.0))
                                    .child(div().flex_1().size_full().child(right_el)),
                            ),
                        )
                        .into_any_element()
                } else {
                    v_resizable(split_id)
                        .child(
                            resizable_panel().size_range(px(70.0)..px(3000.0)).child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .size_full()
                                    .p(px(2.0))
                                    .child(div().flex_1().size_full().child(left_el)),
                            ),
                        )
                        .child(
                            resizable_panel().size_range(px(70.0)..px(3000.0)).child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .size_full()
                                    .p(px(2.0))
                                    .child(div().flex_1().size_full().child(right_el)),
                            ),
                        )
                        .into_any_element()
                }
            }
        }
    }

    /// The pane entity a top-level action (font size) applies to: the focused
    /// pane, or the first pane of the active session's screen.
    fn active_pane_terminal(&self) -> Option<Entity<TerminalView>> {
        let focused_pane = self
            .focused_pane_id
            .and_then(|focused| self.pane_index.get(&focused).copied())
            .and_then(|(p_idx, s_idx, pane_idx)| {
                self.projects
                    .get(p_idx)
                    .and_then(|p| p.sessions.get(s_idx))
                    .and_then(|s| s.panes.get(pane_idx))
            })
            .map(|p| p.terminal.clone());
        focused_pane.or_else(|| {
            self.active_session()
                .and_then(|s| s.panes.first())
                .map(|p| p.terminal.clone())
        })
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        if keystroke.key == "escape" && self.active_view == ActiveView::Settings {
            self.active_view = ActiveView::Workspace;
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if keystroke.modifiers.control && keystroke.key == "," {
            self.active_view = if self.active_view == ActiveView::Settings {
                ActiveView::Workspace
            } else {
                ActiveView::Settings
            };
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
            if let Some(entity) = self.active_pane_terminal() {
                entity.update(cx, |terminal, cx| {
                    let mut config = terminal.config().clone();
                    config.font_size += px(1.0);
                    terminal.update_config(config, cx);
                });
            }
            cx.stop_propagation();
        } else if keystroke.modifiers.control && keystroke.key == "-" {
            if let Some(entity) = self.active_pane_terminal() {
                entity.update(cx, |terminal, cx| {
                    let mut config = terminal.config().clone();
                    if config.font_size > px(6.0) {
                        config.font_size -= px(1.0);
                        terminal.update_config(config, cx);
                    }
                });
            }
            cx.stop_propagation();
        }
    }

    // ---------------------------------------------------------------------
    // Sidebar: projects and their sessions
    // ---------------------------------------------------------------------

    /// The whole left rail: a toolbar, the Projects header with its "Open
    /// project" action, and one collapsible block per open project.
    fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let project_rows: Vec<gpui::AnyElement> = self
            .projects
            .iter()
            .enumerate()
            .map(|(p_idx, project)| self.render_project_entry(p_idx, project, cx))
            .collect();

        let tokens = self.tokens(cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(tokens.sidebar_bg)
            .border_r_1()
            .border_color(tokens.sidebar_border)
            .when(self.project_menu.is_some(), |this| {
                this.child(
                    deferred(
                        div()
                            .id("project-menu-backdrop")
                            .size_full()
                            .absolute()
                            .top_0()
                            .left_0()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.project_menu = None;
                                    cx.notify();
                                }),
                            ),
                    )
                    .priority(100),
                )
            })
            .child(self.render_sidebar_toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .overflow_y_scrollbar()
                    .flex()
                    .flex_col()
                    .pb(px(10.0))
                    .children(project_rows)
                    .when(self.projects.is_empty(), |this| {
                        this.child(self.render_sidebar_empty_state(cx))
                    }),
            )
            .when(self.integration_panel_visible, |this| {
                this.child(self.render_integrations_panel(cx))
            })
            .child(self.render_sidebar_footer(cx))
            .into_any_element()
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let is_settings = self.active_view == ActiveView::Settings;
        let tokens = self.tokens(cx);

        div()
            .id("sidebar-footer")
            .w_full()
            .p(px(8.0))
            .border_t_1()
            .border_color(tokens.sidebar_footer_border)
            .bg(tokens.sidebar_footer_bg)
            .child(
                div()
                    .id("sidebar-settings-btn")
                    .h(px(34.0))
                    .w_full()
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor(CursorStyle::PointingHand)
                    .bg(if is_settings {
                        tokens.sidebar_item_active
                    } else {
                        rgba(0x00000000).into()
                    })
                    .hover(move |s| {
                        if is_settings {
                            s
                        } else {
                            s.bg(tokens.sidebar_item_hover)
                        }
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.active_view = if this.active_view == ActiveView::Settings {
                                ActiveView::Workspace
                            } else {
                                ActiveView::Settings
                            };
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(svg().path("icons/settings.svg").size(px(15.0)).text_color(
                                if is_settings {
                                    tokens.sidebar_item_active_fg
                                } else {
                                    tokens.sidebar_icon
                                },
                            ))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(if is_settings {
                                        gpui::FontWeight::SEMIBOLD
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(if is_settings {
                                        tokens.sidebar_item_active_fg
                                    } else {
                                        tokens.sidebar_fg
                                    })
                                    .child("Settings"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(tokens.sidebar_muted)
                            .child(self.theme_preference.label()),
                    ),
            )
    }

    fn render_settings_page(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let current_pref = self.theme_preference;
        let is_maximized = window.is_maximized();
        let is_fullscreen = window.is_fullscreen();
        let controls = window.window_controls();

        // Window chrome stays available, but the rightmost close button
        // leaves Settings instead of quitting the app.
        let window_controls = (!is_fullscreen).then(|| {
            div()
                .id("settings-window-controls")
                .flex()
                .items_center()
                .h_full()
                .when(controls.minimize, |this| {
                    this.child(
                        div()
                            .id("settings-window-minimize-btn")
                            .w(px(36.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor(CursorStyle::PointingHand)
                            .hover(move |style| style.bg(tokens.titlebar_btn_hover))
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.minimize_window();
                            })
                            .child(
                                svg()
                                    .path("window-minimize.svg")
                                    .size(px(14.0))
                                    .text_color(tokens.titlebar_fg),
                            ),
                    )
                })
                .when(controls.maximize, |this| {
                    this.child(
                        div()
                            .id("settings-window-maximize-btn")
                            .w(px(36.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor(CursorStyle::PointingHand)
                            .hover(move |style| style.bg(tokens.titlebar_btn_hover))
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.zoom_window();
                            })
                            .child(
                                svg()
                                    .path(if is_maximized {
                                        "window-restore.svg"
                                    } else {
                                        "window-maximize.svg"
                                    })
                                    .size(px(14.0))
                                    .text_color(tokens.titlebar_fg),
                            ),
                    )
                })
                .child(
                    div()
                        .id("settings-close-btn")
                        .debug_selector(|| "settings-close-btn".to_string())
                        .w(px(36.0))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor(CursorStyle::PointingHand)
                        .hover(move |style| style.bg(tokens.titlebar_btn_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.active_view = ActiveView::Workspace;
                                cx.notify();
                            }),
                        )
                        .child(
                            svg()
                                .path("window-close.svg")
                                .size(px(14.0))
                                .text_color(tokens.titlebar_fg),
                        ),
                )
        });

        let titlebar = self
            .make_drag_area(cx)
            .h(px(38.0))
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .pl(px(14.0))
            .pr(px(10.0))
            .bg(tokens.titlebar_bg)
            .border_b_1()
            .border_color(tokens.titlebar_border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        svg()
                            .path("icons/settings.svg")
                            .size(px(14.0))
                            .text_color(tokens.titlebar_fg),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(tokens.titlebar_fg)
                            .child("Settings"),
                    ),
            )
            .children(window_controls);

        let options = [
            (
                ThemePreference::Light,
                "theme-option-light",
                "Light",
                "Bright and crisp",
                "icons/sun.svg",
            ),
            (
                ThemePreference::Dark,
                "theme-option-dark",
                "Dark",
                "Easy on the eyes",
                "icons/moon.svg",
            ),
            (
                ThemePreference::System,
                "theme-option-system",
                "System",
                "Match your desktop",
                "icons/monitor.svg",
            ),
        ];

        let cards = options
            .into_iter()
            .map(|(preference, id, title, subtitle, icon)| {
                let selected = current_pref == preference;
                div()
                    .id(id)
                    .flex_1()
                    .min_w(px(0.0))
                    .p(px(16.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(if selected {
                        tokens.ring
                    } else {
                        tokens.card_border
                    })
                    .bg(if selected {
                        tokens.card_bg
                    } else {
                        tokens.pill_inactive_bg
                    })
                    .cursor(CursorStyle::PointingHand)
                    .hover(move |style| {
                        if selected {
                            style
                        } else {
                            style.bg(tokens.card_hover)
                        }
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.set_theme_preference(preference, Some(window), cx);
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(14.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .size(px(34.0))
                                            .rounded(px(8.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .bg(if selected {
                                                tokens.pill_active_bg
                                            } else {
                                                tokens.pill_inactive_bg
                                            })
                                            .child(svg().path(icon).size(px(17.0)).text_color(
                                                if selected {
                                                    tokens.pill_active_fg
                                                } else {
                                                    tokens.text_secondary
                                                },
                                            )),
                                    )
                                    .when(selected, |this| {
                                        this.child(
                                            div()
                                                .size(px(18.0))
                                                .rounded_full()
                                                .bg(tokens.pill_active_bg)
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(
                                                    svg()
                                                        .path("icons/status-check.svg")
                                                        .size(px(11.0))
                                                        .text_color(tokens.pill_active_fg),
                                                ),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .child(
                                        div()
                                            .text_size(px(13.5))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(tokens.text_primary)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(tokens.text_secondary)
                                            .child(subtitle),
                                    ),
                            ),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(tokens.window_bg)
            .child(titlebar)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_y_scrollbar()
                    .p(px(24.0))
                    .child(
                        div()
                            .w_full()
                            .max_w(px(640.0))
                            .flex()
                            .flex_col()
                            .gap(px(18.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(
                                        div()
                                            .text_size(px(20.0))
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .text_color(tokens.text_primary)
                                            .child("Appearance"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .text_color(tokens.text_secondary)
                                            .child("Choose how Kathamo looks."),
                                    ),
                            )
                            .child(div().flex().gap(px(12.0)).children(cards))
                            .child(self.render_palette_scheme_picker(cx))
                            .child(self.render_data_section(cx)),
                    ),
            )
    }

    /// Data directory, restore toggles and the "clear saved sessions" action.
    fn render_data_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let restore = self.settings.restore.clone();
        let toggles = [
            (
                "restore-sessions",
                "Restore sessions on launch",
                "Reopen projects, tabs, splits, focus and working directories.",
                restore.sessions,
            ),
            (
                "restore-scrollback",
                "Restore terminal scrollback",
                "Seed the previous screen text above the fresh shell prompt.",
                restore.scrollback,
            ),
            (
                "restore-agent-history",
                "Save agent history",
                "Keep conversation transcripts and resume rpi sessions after a restart.",
                restore.agent_history,
            ),
        ];
        let toggle_rows = toggles
            .into_iter()
            .map(|(id, title, subtitle, on)| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_restore_option(id, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(tokens.text_primary)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(tokens.text_secondary)
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded_full()
                            .bg(if on {
                                tokens.pill_active_bg
                            } else {
                                tokens.pill_inactive_bg
                            })
                            .text_size(px(11.0))
                            .text_color(if on {
                                tokens.pill_active_fg
                            } else {
                                tokens.text_secondary
                            })
                            .child(if on { "On" } else { "Off" }),
                    )
            })
            .collect::<Vec<_>>();

        let confirm = self.clear_data_confirm;
        let data_dir = paths::data_dir();
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(tokens.text_primary)
                            .child("Data & Sessions"),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(tokens.text_secondary)
                            .child("Everything Kathamo remembers is stored in this folder."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(tokens.text_secondary)
                                    .child("App data folder"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(tokens.text_primary)
                                    .child(data_dir.display().to_string()),
                            ),
                    )
                    .child(
                        div()
                            .id("open-data-folder")
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(tokens.card_border)
                            .text_size(px(12.0))
                            .text_color(tokens.text_primary)
                            .cursor(CursorStyle::PointingHand)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_data_folder(cx);
                            }))
                            .child("Open folder"),
                    ),
            )
            .children(toggle_rows)
            .child(
                div()
                    .id("clear-saved-data")
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clear_saved_data(cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(tokens.block_failure)
                                    .child("Clear saved sessions & history"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(tokens.text_secondary)
                                    .child("Deletes saved layouts, scrollback and transcripts. Settings are kept."),
                            ),
                    )
                    .child(
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded_full()
                            .bg(tokens.pill_inactive_bg)
                            .text_size(px(11.0))
                            .text_color(if confirm {
                                tokens.block_failure
                            } else {
                                tokens.text_secondary
                            })
                            .child(if confirm { "Confirm clear" } else { "Clear" }),
                    ),
            )
    }

    fn toggle_restore_option(&mut self, id: &'static str, cx: &mut Context<Self>) {
        match id {
            "restore-sessions" => self.settings.restore.sessions = !self.settings.restore.sessions,
            "restore-scrollback" => {
                self.settings.restore.scrollback = !self.settings.restore.scrollback
            }
            "restore-agent-history" => {
                self.settings.restore.agent_history = !self.settings.restore.agent_history
            }
            _ => return,
        }
        self.clear_data_confirm = false;
        if let Err(error) = settings::save_atomic(&self.settings) {
            eprintln!("failed to save settings: {error}");
        }
        cx.notify();
    }

    fn open_data_folder(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = paths::open_in_file_manager(&paths::data_dir()) {
            eprintln!("failed to open the data folder: {error}");
        }
        cx.notify();
    }

    fn clear_saved_data(&mut self, cx: &mut Context<Self>) {
        if !self.clear_data_confirm {
            self.clear_data_confirm = true;
            cx.notify();
            return;
        }
        self.clear_data_confirm = false;
        if let Err(error) = std::fs::remove_file(paths::workspace_path()) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("failed to remove saved workspace: {error}");
            }
        }
        let _ = std::fs::remove_dir_all(paths::sessions_dir());
        let _ = std::fs::create_dir_all(paths::sessions_dir());
        if let Err(error) = conversations::clear_all() {
            eprintln!("failed to clear conversations: {error}");
        }
        self.dirty = false;
        cx.notify();
    }

    /// Warp / GitHub terminal palette selector shown under the theme cards.
    fn render_palette_scheme_picker(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let current = self.palette_scheme;
        let options = [
            (PaletteScheme::Warp, "palette-scheme-warp"),
            (PaletteScheme::Github, "palette-scheme-github"),
        ];
        let pills = options
            .into_iter()
            .map(|(scheme, id)| {
                let selected = current == scheme;
                div()
                    .id(id)
                    .flex_1()
                    .min_w(px(0.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if selected {
                        tokens.ring
                    } else {
                        tokens.card_border
                    })
                    .bg(if selected {
                        tokens.card_bg
                    } else {
                        tokens.pill_inactive_bg
                    })
                    .cursor(CursorStyle::PointingHand)
                    .hover(move |style| {
                        if selected {
                            style
                        } else {
                            style.bg(tokens.card_hover)
                        }
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.set_palette_scheme(scheme, Some(window), cx);
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(tokens.text_primary)
                                            .child(scheme.label()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(tokens.text_secondary)
                                            .child(scheme.description()),
                                    ),
                            )
                            .when(selected, |this| {
                                this.child(
                                    svg()
                                        .path("icons/status-check.svg")
                                        .size(px(14.0))
                                        .text_color(tokens.ring),
                                )
                            }),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(tokens.text_primary)
                            .child("Terminal palette"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(tokens.text_secondary)
                            .child("Warp reproduces the Warp terminal's colors and block style."),
                    ),
            )
            .child(div().flex().gap(px(12.0)).children(pills))
    }

    fn render_sidebar_toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        div()
            .h(px(38.0))
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .pl(px(14.0))
            .pr(px(10.0))
            .child(sidebar_icon_button(
                "sidebar-toggle-btn",
                "icons/panel-left.svg",
                16.0,
                &tokens,
                cx.listener::<MouseDownEvent>(|this, _, _, cx| {
                    this.sidebar_visible = !this.sidebar_visible;
                    cx.notify();
                    this.schedule_workspace_save(cx);
                }),
            ))
            .child(self.make_drag_area(cx).flex_1().h_full())
            .child(div().flex().items_center().child(sidebar_icon_button(
                "sidebar-open-project-btn",
                "icons/folder-plus.svg",
                15.0,
                &tokens,
                cx.listener::<MouseDownEvent>(|this, _, window, cx| {
                    this.select_folder_and_add_project(window, cx);
                }),
            )))
    }

    fn render_integrations_panel(&self, cx: &mut Context<Self>) -> gpui::Div {
        let weak = cx.entity().downgrade();
        let tokens = self.tokens(cx);
        div()
            .w_full()
            .px(px(12.0))
            .py(px(10.0))
            .border_t_1()
            .border_color(tokens.sidebar_border)
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(div().text_size(px(12.0)).font_weight(gpui::FontWeight::BOLD).text_color(tokens.sidebar_fg).child("Agent integrations"))
            .child(div().text_size(px(10.5)).text_color(tokens.sidebar_muted).child("Claude Code · Codex · OpenCode · Pi · OMP · Command Code"))
            .child(div().text_size(px(10.0)).text_color(tokens.sidebar_muted).child("Versions and hook trust: kathamo integration doctor"))
            .child(div().text_size(px(10.0)).text_color(tokens.sidebar_muted).child(self.integration_feedback.clone()))
            .child(
                div()
                    .id("enable-agent-integrations")
                    .w_full().py(px(6.0)).rounded(px(6.0))
                    .bg(tokens.card_bg).hover(move |s| s.bg(tokens.card_hover))
                    .cursor(CursorStyle::PointingHand)
                    .text_size(px(11.0)).text_color(tokens.sidebar_fg)
                    .flex().items_center().justify_center()
                    .child("Enable local integrations")
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        let target = weak.clone();
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            let result = cx.background_executor().spawn(async move {
                                std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| {
                                    std::process::Command::new(exe).args(["integration", "setup"]).output().map_err(anyhow::Error::from)
                                })
                            }).await;
                            let message = match result {
                                Ok(output) if output.status.success() => "Installed. Restart each agent; Codex may also ask you to trust its hooks.".to_string(),
                                Ok(output) => format!("Setup failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
                                Err(error) => format!("Setup failed: {error}"),
                            };
                            let _ = target.update(cx, |this, cx| { this.integration_feedback = SharedString::from(message); cx.notify(); });
                        }).detach();
                    }),
            )
    }

    /// One collapsible project block: the project header (activate, expand,
    /// new session, close project) followed by its session rows.
    fn render_project_entry(
        &self,
        project_idx: usize,
        project: &Project,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tokens = self.tokens(cx);
        let is_active_project = project_idx == self.active_project_idx;
        let needs_attention = project
            .sessions
            .iter()
            .filter(|s| matches!(s.status, SessionStatus::Blocked | SessionStatus::Error))
            .count();
        let chevron = if project.expanded {
            "icons/chevron-down.svg"
        } else {
            "icons/chevron-right.svg"
        };

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if project.expanded {
            if project.sessions.is_empty() {
                rows.push(
                    div()
                        .pl(px(34.0))
                        .pr(px(16.0))
                        .py(px(6.0))
                        .text_size(px(11.5))
                        .text_color(tokens.sidebar_muted)
                        .child("No sessions yet")
                        .into_any_element(),
                );
            }
            for (s_idx, session) in project.sessions.iter().enumerate() {
                let is_active_session = is_active_project && s_idx == project.active_session_idx;
                rows.push(self.render_session_entry(
                    project_idx,
                    s_idx,
                    session,
                    is_active_session,
                    cx,
                ));
            }
        }

        let is_renaming = self
            .renaming_project
            .as_ref()
            .is_some_and(|r| r.project_id == project.id);
        let is_menu_open = self.project_menu == Some(project.id);
        let group_name = SharedString::from(format!("project-row-{}", project.id));
        let project_id = project.id;

        let header_content: gpui::AnyElement = if is_renaming {
            let renaming = self.renaming_project.as_ref().unwrap();
            let input_entity = renaming.input.clone();
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&renaming.input)
                            .with_size(gpui_component::Size::Small)
                            .cleanable(false),
                    ),
                )
                .child(sidebar_icon_button(
                    ("project-rename-save", project.id),
                    "icons/status-check.svg",
                    12.0,
                    &tokens,
                    cx.listener(move |this, _, _, cx| {
                        let val = input_entity.read(cx).value();
                        this.finish_renaming_project(project_id, val, cx);
                    }),
                ))
                .child(sidebar_icon_button(
                    ("project-rename-cancel", project.id),
                    "icons/x.svg",
                    11.0,
                    &tokens,
                    cx.listener(move |this, _, _, cx| {
                        this.cancel_renaming_project(cx);
                    }),
                ))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if is_active_project {
                            tokens.sidebar_fg
                        } else {
                            tokens.sidebar_muted
                        })
                        .child(project.name.clone()),
                )
                .child(
                    div()
                        .relative()
                        .child(
                            sidebar_icon_button(
                                ("project-options", project.id),
                                "icons/ellipsis.svg",
                                13.0,
                                &tokens,
                                cx.listener::<MouseDownEvent>(move |this, _, _, cx| {
                                    this.project_menu = if this.project_menu == Some(project_id) {
                                        None
                                    } else {
                                        Some(project_id)
                                    };
                                    cx.notify();
                                }),
                            )
                            .opacity(if is_menu_open { 1.0 } else { 0.0 })
                            .group_hover(group_name.clone(), |s| s.opacity(1.0)),
                        )
                        .when(is_menu_open, |this| {
                            this.child(
                                deferred(
                                    div()
                                        .id(("project-menu-popover", project.id))
                                        .absolute()
                                        .top(px(24.0))
                                        .left_0()
                                        .w(px(130.0))
                                        .bg(tokens.card_bg)
                                        .border_1()
                                        .border_color(tokens.card_border)
                                        .rounded(px(6.0))
                                        .shadow_md()
                                        .p(px(4.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.0))
                                        .child(
                                            div()
                                                .id(("menu-rename", project.id))
                                                .px(px(8.0))
                                                .py(px(5.0))
                                                .rounded(px(4.0))
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_primary)
                                                .cursor(CursorStyle::PointingHand)
                                                .hover(move |s| s.bg(tokens.sidebar_item_hover))
                                                .child("Rename")
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        this.start_renaming_project(
                                                            project_id, window, cx,
                                                        );
                                                    }),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .id(("menu-remove", project.id))
                                                .px(px(8.0))
                                                .py(px(5.0))
                                                .rounded(px(4.0))
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_primary)
                                                .cursor(CursorStyle::PointingHand)
                                                .hover(move |s| s.bg(tokens.sidebar_item_hover))
                                                .child("Remove")
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        this.project_menu = None;
                                                        if let Some(idx) = this
                                                            .projects
                                                            .iter()
                                                            .position(|p| p.id == project_id)
                                                        {
                                                            this.close_project(idx, window, cx);
                                                        }
                                                    }),
                                                ),
                                        ),
                                )
                                .priority(101),
                            )
                        }),
                )
                .child(div().flex_1())
                .when(needs_attention > 0, |this| {
                    this.child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgba(0xb7782fff))
                            .child(format!("{needs_attention} needs you")),
                    )
                })
                .child(
                    sidebar_icon_button(
                        ("project-new-session", project.id),
                        "icons/plus.svg",
                        13.0,
                        &tokens,
                        cx.listener::<MouseDownEvent>(move |this, _, window, cx| {
                            this.create_new_session_in_project(project_idx, window, cx);
                        }),
                    )
                    .debug_selector(|| "project-new-session".to_string()),
                )
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .mb(px(18.0))
            .child(
                div()
                    .id(("project-row", project.id))
                    .group(group_name)
                    .ml(px(6.0))
                    .mr(px(6.0))
                    .mt(px(2.0))
                    .h(px(32.0))
                    .pl(px(4.0))
                    .pr(px(4.0))
                    .rounded(px(6.0))
                    .cursor(CursorStyle::PointingHand)
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .bg(rgba(0x00000000))
                    .hover(move |s| s.bg(tokens.sidebar_item_hover))
                    .child(
                        div()
                            .id(("project-chevron", project.id))
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .hover(move |s| s.bg(tokens.sidebar_icon_btn_hover))
                            .child(
                                svg()
                                    .path(chevron)
                                    .size(px(12.0))
                                    .text_color(tokens.sidebar_icon),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener::<MouseDownEvent>(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_project_expanded(project_idx, cx);
                                }),
                            ),
                    )
                    .child(header_content)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener::<MouseDownEvent>(move |this, _, window, cx| {
                            if this.renaming_project.as_ref().map(|r| r.project_id)
                                == Some(project_id)
                            {
                                return;
                            }
                            this.activate_project(project_idx, window, cx);
                        }),
                    ),
            )
            .children(rows)
            .into_any_element()
    }

    /// One session (screen) inside a project: logo, live status, title and a
    /// close action. Clicking it swaps the workspace to that session.
    fn render_session_entry(
        &self,
        project_idx: usize,
        session_idx: usize,
        session: &Session,
        is_active_session: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tokens = self.tokens(cx);
        let selected_evidence = session
            .panes
            .iter()
            .max_by_key(|pane| pane.status.rank())
            .map(|pane| {
                format!(
                    "{} — {:?} evidence, {:?} confidence",
                    pane.status.label(),
                    pane.detection.evidence,
                    pane.detection.confidence
                )
            })
            .unwrap_or_else(|| format!("{} — no pane evidence", session.status.label()));
        let indicator = match session.status {
            SessionStatus::Working => gpui_component::Icon::new(gpui_component::IconName::Loader)
                .size(px(16.0))
                .text_color(tokens.sidebar_muted)
                .with_animation(
                    ("session-spinner", session.id),
                    Animation::new(std::time::Duration::from_millis(900)).repeat(),
                    |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                )
                .into_any_element(),
            SessionStatus::Done => div()
                .id(("session-status-ack", session.id))
                .size(px(19.0))
                .rounded_full()
                .bg(rgba(0x43b976dd))
                .border_1()
                .border_color(rgba(0xc1f7d8ff))
                .flex()
                .items_center()
                .justify_center()
                .cursor(CursorStyle::PointingHand)
                .tooltip(|_, cx| {
                    cx.new(|_| {
                        gpui_component::tooltip::Tooltip::new(SharedString::from(
                            "Clear completed status",
                        ))
                    })
                    .into()
                })
                .child(
                    svg()
                        .path("icons/status-check.svg")
                        .size(px(12.0))
                        .text_color(rgba(0xffffffff)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(session) = this
                            .projects
                            .get_mut(project_idx)
                            .and_then(|project| project.sessions.get_mut(session_idx))
                        {
                            for pane in &mut session.panes {
                                if matches!(pane.status, SessionStatus::Done | SessionStatus::Error)
                                {
                                    pane.status = SessionStatus::Idle;
                                    pane.native_status = None;
                                    pane.native_acknowledged = true;
                                    pane.attention.seen = true;
                                    pane.attention.last = Some(SessionStatus::Idle);
                                    pane.ack_screen =
                                        Some(screen_fingerprint(&pane.screen_scratch, &pane.title));
                                    pane.osc_report_active = false;
                                    pane.pending_approvals.clear();
                                    pane.poll_cache = None;
                                    pane.detection = status::DetectionResult::new(
                                        SessionStatus::Idle,
                                        status::EvidenceSource::NoSignal,
                                        status::Confidence::None,
                                    );
                                    pane.terminal.update(cx, |terminal, _| {
                                        terminal.clear_agent_status_reports()
                                    });
                                }
                            }
                            session.status = session
                                .panes
                                .iter()
                                .map(|pane| pane.status)
                                .max_by_key(|status| status.rank())
                                .unwrap_or(SessionStatus::Idle);
                        }
                        cx.notify();
                    }),
                )
                .into_any_element(),
            SessionStatus::Error => div()
                .id(("session-error-ack", session.id))
                .size(px(19.0))
                .rounded_full()
                .bg(rgba(0xf85149dd))
                .border_1()
                .border_color(rgba(0xffc7c4ff))
                .flex()
                .items_center()
                .justify_center()
                .cursor(CursorStyle::PointingHand)
                .tooltip(|_, cx| {
                    cx.new(|_| {
                        gpui_component::tooltip::Tooltip::new(SharedString::from(
                            "Clear failed status",
                        ))
                    })
                    .into()
                })
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgba(0xffffffff))
                        .child("!"),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(session) = this
                            .projects
                            .get_mut(project_idx)
                            .and_then(|project| project.sessions.get_mut(session_idx))
                        {
                            for pane in &mut session.panes {
                                if matches!(pane.status, SessionStatus::Done | SessionStatus::Error)
                                {
                                    pane.status = SessionStatus::Idle;
                                    pane.native_status = None;
                                    pane.native_acknowledged = true;
                                    pane.attention.seen = true;
                                    pane.attention.last = Some(SessionStatus::Idle);
                                    pane.ack_screen =
                                        Some(screen_fingerprint(&pane.screen_scratch, &pane.title));
                                    pane.osc_report_active = false;
                                    pane.pending_approvals.clear();
                                    pane.poll_cache = None;
                                    pane.detection = status::DetectionResult::new(
                                        SessionStatus::Idle,
                                        status::EvidenceSource::NoSignal,
                                        status::Confidence::None,
                                    );
                                    pane.terminal.update(cx, |terminal, _| {
                                        terminal.clear_agent_status_reports()
                                    });
                                }
                            }
                            session.status = session
                                .panes
                                .iter()
                                .map(|pane| pane.status)
                                .max_by_key(|status| status.rank())
                                .unwrap_or(SessionStatus::Idle);
                        }
                        cx.notify();
                    }),
                )
                .into_any_element(),
            SessionStatus::Blocked => div()
                .size(px(19.0))
                .rounded_full()
                .bg(rgba(0xffedc9cc))
                .border_1()
                .border_color(rgba(0xc8873cff))
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(7.0)).rounded_full().bg(rgba(0xc8873cff)))
                .into_any_element(),
            SessionStatus::Unknown => div()
                .flex()
                .items_center()
                .text_size(px(10.0))
                .text_color(tokens.sidebar_muted)
                .child("Unknown")
                .into_any_element(),
            SessionStatus::Active | SessionStatus::Idle => div().size(px(19.0)).into_any_element(),
        };

        div()
            .id(("session-item", session.id))
            .ml(px(6.0))
            .mr(px(6.0))
            .h(px(38.0))
            .pl(px(12.0))
            .pr(px(4.0))
            .rounded(px(6.0))
            .cursor(CursorStyle::PointingHand)
            .tooltip(move |_, cx| {
                let text = selected_evidence.clone();
                cx.new(|_| gpui_component::tooltip::Tooltip::new(SharedString::from(text)))
                    .into()
            })
            .flex()
            .items_center()
            .gap(px(8.0))
            .bg(if is_active_session {
                tokens.sidebar_item_active
            } else {
                rgba(0x00000000).into()
            })
            .hover(move |s| {
                if is_active_session {
                    s
                } else {
                    s.bg(tokens.sidebar_item_hover)
                }
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .font_weight(
                        if matches!(
                            session.status,
                            SessionStatus::Blocked | SessionStatus::Error
                        ) {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::MEDIUM
                        },
                    )
                    .text_color(if is_active_session {
                        tokens.sidebar_item_active_fg
                    } else {
                        tokens.sidebar_fg
                    })
                    .child(session.display_title.clone()),
            )
            .child(indicator)
            .child(
                sidebar_icon_button(
                    ("session-close", session.id),
                    "icons/x.svg",
                    10.0,
                    &tokens,
                    cx.listener::<MouseDownEvent>(move |this, _, window, cx| {
                        this.close_session(project_idx, session_idx, window, cx);
                    }),
                )
                .debug_selector(|| "session-close".to_string()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener::<MouseDownEvent>(move |this, _, window, cx| {
                    this.select_session(project_idx, session_idx, window, cx);
                }),
            )
            .into_any_element()
    }

    fn render_sidebar_empty_state(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        div()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(36.0))
            .px(px(20.0))
            .gap(px(8.0))
            .child(
                div()
                    .size(px(40.0))
                    .rounded(px(10.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(tokens.card_bg)
                    .child(
                        svg()
                            .path("icons/folder.svg")
                            .size(px(20.0))
                            .text_color(tokens.sidebar_icon),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(tokens.sidebar_fg)
                    .child("No projects open"),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(tokens.sidebar_muted)
                    .text_center()
                    .child("Open any folder to start a project with its own sessions."),
            )
            .child(sidebar_text_button(
                "sidebar-empty-open-project",
                "Open project",
                "icons/folder-plus.svg",
                true,
                &tokens,
                cx.listener::<MouseDownEvent>(|this, _, window, cx| {
                    this.select_folder_and_add_project(window, cx);
                }),
            ))
    }

    /// Shown in the terminal area when the active project has no session, or
    /// when no project is open at all.
    fn render_empty_terminal(&self, cx: &mut Context<Self>) -> gpui::Div {
        let has_project = self.active_project().is_some();
        let project_idx = self.active_project_idx;
        let tokens = self.tokens(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .child(
                div()
                    .size(px(48.0))
                    .rounded(px(12.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(tokens.card_bg)
                    .child(
                        svg()
                            .path("icons/folder.svg")
                            .size(px(24.0))
                            .text_color(tokens.text_secondary),
                    ),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(tokens.text_primary)
                    .child(if has_project {
                        "No session in this project"
                    } else {
                        "No project open"
                    }),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(tokens.text_muted)
                    .child(if has_project {
                        "Create a session to start a terminal in this project."
                    } else {
                        "Open a folder to start a project with its own sessions."
                    }),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(sidebar_text_button(
                        "empty-terminal-open-project",
                        "Open project",
                        "icons/folder-plus.svg",
                        true,
                        &tokens,
                        cx.listener::<MouseDownEvent>(|this, _, window, cx| {
                            this.select_folder_and_add_project(window, cx);
                        }),
                    ))
                    .when(has_project, |this| {
                        this.child(sidebar_text_button(
                            "empty-terminal-new-session",
                            "New session",
                            "icons/plus.svg",
                            false,
                            &tokens,
                            cx.listener::<MouseDownEvent>(move |this, _, window, cx| {
                                this.create_new_session_in_project(project_idx, window, cx);
                            }),
                        ))
                    }),
            )
    }

    /// Switch the workspace to a session in a given project.
    fn select_session(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_view = ActiveView::Workspace;
        let Some(project) = self.projects.get_mut(project_idx) else {
            return;
        };
        if session_idx >= project.sessions.len() {
            return;
        }
        project.active_session_idx = session_idx;
        project.expanded = true;
        self.active_project_idx = project_idx;
        let pane_id = project.sessions[session_idx].panes.first().map(|p| p.id);
        if let Some(pane_id) = pane_id {
            self.focus_pane(pane_id, window, cx);
        } else {
            self.focused_pane_id = None;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }
}

/// Small square icon button used by the sidebar rows and the sidebar
/// toolbar. `on_click` is a `cx.listener`; the button stops mouse propagation
/// so clicking it never also activates the row it sits in.
fn sidebar_icon_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    icon_size: f32,
    tokens: &ThemeTokens,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let hover_bg = tokens.sidebar_icon_btn_hover;
    let icon_color = tokens.sidebar_icon;
    let icon_hover_color = tokens.sidebar_icon_hover;
    div()
        .id(id)
        .size(px(20.0))
        .rounded(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor(CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover_bg))
        .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
        .child(
            svg()
                .path(icon)
                .size(px(icon_size))
                .text_color(icon_color)
                .hover(move |s| s.text_color(icon_hover_color)),
        )
}

fn sidebar_text_button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    icon: &'static str,
    primary: bool,
    tokens: &ThemeTokens,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let bg_color = if primary {
        tokens.primary_btn_bg
    } else {
        tokens.secondary_btn_bg
    };
    let border_color = if primary {
        tokens.primary_btn_bg
    } else {
        tokens.border
    };
    let hover_color = if primary {
        tokens.primary_btn_hover
    } else {
        tokens.secondary_btn_hover
    };
    let text_color = if primary {
        tokens.primary_btn_fg
    } else {
        tokens.secondary_btn_fg
    };
    div()
        .id(id)
        .h(px(30.0))
        .px(px(12.0))
        .rounded(px(6.0))
        .cursor(CursorStyle::PointingHand)
        .flex()
        .items_center()
        .gap(px(6.0))
        .bg(bg_color)
        .border_1()
        .border_color(border_color)
        .hover(move |s| s.bg(hover_color))
        .child(svg().path(icon).size(px(13.0)).text_color(text_color))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(text_color)
                .child(label),
        )
        .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
}

impl Render for AppState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let diag_render = diagnostics::render_start();
        // "Looking" needs an active window: a visible but backgrounded window
        // has not been seen, so a finish there must still become Done.
        self.window_active = _window.is_window_active();
        self.ensure_panes_input_state(_window, cx);

        let tokens = self.tokens(cx);
        // Track which pane owns keyboard focus each frame; the poller uses
        // this for Active-state resolution and for clearing sticky Done/Error.
        let mut focused_pane = None;
        if let Some(session) = self.active_session() {
            for pane in &session.panes {
                let is_term_focused = pane.terminal.read(cx).focus_handle().is_focused(_window);
                let is_input_focused = pane
                    .input_state
                    .as_ref()
                    .is_some_and(|input| input.read(cx).focus_handle().is_focused(_window));
                if is_term_focused || is_input_focused {
                    focused_pane = Some(pane.id);
                    break;
                }
            }
        }
        if let Some(focused_pane) = focused_pane {
            self.focused_pane_id = Some(focused_pane);
        } else if self.focused_pane_id.is_none() {
            self.focused_pane_id = self
                .active_session()
                .and_then(|s| s.panes.first())
                .map(|p| p.id);
        }

        // Drain alerts here, where a window exists to take notifications and
        // sounds; the poller has none. A blocked transition while looking only
        // rings (no toast), and a missed finish rings only when it happened.
        let alerts = std::mem::take(&mut self.pending_alerts);
        if !alerts.is_empty() {
            let weak = cx.entity().downgrade();
            _window.defer(cx, move |window, cx| {
                for alert in alerts {
                    crate::sound::play(alert.kind);
                    if alert.kind == attention::AlertKind::Blocked && alert.looking {
                        continue;
                    }
                    let (title, kind) = match alert.kind {
                        attention::AlertKind::Blocked => {
                            (format!("{} needs you", alert.name), NotificationType::Error)
                        }
                        attention::AlertKind::Done => {
                            (format!("{} finished", alert.name), NotificationType::Info)
                        }
                    };
                    let weak = weak.clone();
                    let pane_id = alert.pane_id;
                    let note = Notification::new()
                        .title(title)
                        .message(alert.place)
                        .with_type(kind)
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.focus_pane(pane_id, window, cx);
                            });
                        });
                    window.push_notification(note, cx);
                }
            });
        }

        let active_title = self
            .active_session()
            .map(|s| s.title.clone())
            .unwrap_or_else(|| SharedString::from("No session"));

        let active_project_name = self
            .active_project()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| SharedString::from("No project"));

        let terminal_area = if let Some(session) = self.active_session() {
            // Each session is its own screen: its pane tree (and maximized
            // pane) is rendered exclusively; other sessions' panes keep
            // running in the background.
            if let Some(max_pane_id) = session.maximized_pane_id {
                if let Some(pane) = session.panes.iter().find(|p| p.id == max_pane_id) {
                    self.render_terminal_pane(pane, true, _window, cx)
                        .into_any_element()
                } else if let Some(tree) = &session.pane_tree {
                    self.render_pane_node(tree, session, false, _window, cx)
                } else {
                    div().into_any_element()
                }
            } else if let Some(tree) = &session.pane_tree {
                self.render_pane_node(tree, session, false, _window, cx)
            } else if let Some(pane) = session.panes.first() {
                self.render_terminal_pane(pane, false, _window, cx)
                    .into_any_element()
            } else {
                div().into_any_element()
            }
        } else {
            self.render_empty_terminal(cx).into_any_element()
        };
        // Focused terminal's status and logo, mirrored in the title bar.
        let titlebar_status = self.active_session().map(|s| s.status);
        let titlebar_logo = self.active_session().map(|s| s.logo.clone());

        let is_maximized = _window.is_maximized();
        let is_fullscreen = _window.is_fullscreen();
        let controls = _window.window_controls();

        let default_window_controls = (!is_fullscreen).then(|| {
            div()
                .id("default-window-controls")
                .flex()
                .items_center()
                .h_full()
                .when(controls.minimize, |this| {
                    this.child(
                        div()
                            .id("window-minimize-btn")
                            .w(px(36.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor(CursorStyle::PointingHand)
                            .hover(move |s| s.bg(tokens.titlebar_btn_hover))
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.minimize_window();
                            })
                            .child(
                                svg()
                                    .path("window-minimize.svg")
                                    .size(px(14.0))
                                    .text_color(tokens.titlebar_fg)
                                    .hover(move |s| s.text_color(tokens.text_primary)),
                            ),
                    )
                })
                .when(controls.maximize, |this| {
                    this.child(
                        div()
                            .id("window-maximize-btn")
                            .w(px(36.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor(CursorStyle::PointingHand)
                            .hover(move |s| s.bg(tokens.titlebar_btn_hover))
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.zoom_window();
                            })
                            .child(
                                svg()
                                    .path(if is_maximized {
                                        "window-restore.svg"
                                    } else {
                                        "window-maximize.svg"
                                    })
                                    .size(px(14.0))
                                    .text_color(tokens.titlebar_fg)
                                    .hover(move |s| s.text_color(tokens.text_primary)),
                            ),
                    )
                })
                .child(
                    div()
                        .id("window-close-btn")
                        .w(px(36.0))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor(CursorStyle::PointingHand)
                        .hover(|s| s.bg(rgba(0xe81123ff)))
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            cx.stop_propagation();
                            window.remove_window();
                        })
                        .child(
                            svg()
                                .path("window-close.svg")
                                .size(px(14.0))
                                .text_color(tokens.titlebar_fg)
                                .hover(|s| s.text_color(rgba(0xffffffff))),
                        ),
                )
        });

        let terminal_content = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(tokens.window_bg)
            .child(
                self.make_drag_area(cx)
                    .h(px(38.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl_4()
                    .bg(tokens.titlebar_bg)
                    .border_b_1()
                    .border_color(tokens.titlebar_border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!self.sidebar_visible, |this| {
                                this.child(
                                    div()
                                        .id("terminal-sidebar-toggle-btn")
                                        .cursor(CursorStyle::PointingHand)
                                        .mr_2()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            svg()
                                                .path("icons/panel-left.svg")
                                                .size(px(16.0))
                                                .text_color(tokens.titlebar_fg)
                                                .hover(move |s| s.text_color(tokens.text_primary)),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.sidebar_visible = true;
                                                cx.notify();
                                                this.schedule_workspace_save(cx);
                                            }),
                                        ),
                                )
                            })
                            .text_size(px(12.5))
                            .text_color(tokens.titlebar_fg)
                            .children(titlebar_logo.map(|logo| {
                                div()
                                    .size(px(16.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .flex_shrink_0()
                                    .child(gpui::img(logo).size(px(14.0)).rounded(px(2.0)))
                            }))
                            .child(format!("{active_project_name} — {active_title}"))
                            .children(titlebar_status.map(|status| {
                                let (dot_color, text_color, badge_bg) = status.colors();
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(rgba(badge_bg))
                                    .text_color(rgba(text_color))
                                    .child(div().size(px(6.0)).rounded_full().bg(rgba(dot_color)))
                                    .child(status.label())
                            })),
                    )
                    .children(default_window_controls),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .size_full()
                    .bg(tokens.window_bg)
                    .overflow_hidden()
                    .p(px(6.0))
                    .child(div().flex().flex_col().size_full().child(terminal_area)),
            );

        let sidebar_content = self.render_sidebar(cx);

        let main_content = match self.active_view {
            ActiveView::Workspace => terminal_content.into_any_element(),
            ActiveView::Settings => self.render_settings_page(_window, cx).into_any_element(),
        };

        let root = div()
            .id("app-root")
            .flex()
            .size_full()
            .bg(tokens.window_bg)
            .text_color(tokens.text_primary)
            .on_key_down(cx.listener(Self::on_key_down))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let keystroke = &event.keystroke;

                if this.image_viewer.is_some() {
                    if keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.image_viewer = None;
                        cx.notify();
                    }
                    return;
                }
                if let Some(pane_id) = this.focused_pane_id {
                    if this
                        .pane_ref(pane_id)
                        .is_some_and(|pane| pane.chat.auth.is_some())
                    {
                        if keystroke.key == "escape" {
                            cx.stop_propagation();
                            this.close_agent_auth(pane_id, window, cx);
                        }
                        return;
                    }
                }

                if this.active_view != ActiveView::Workspace
                    || this.renaming_project.is_some()
                    || this.history_search.is_some()
                {
                    return;
                }

                // Intercept paste anywhere in the workspace — the input bar
                // and the terminal grid — so clipboard images (or copied
                // image paths) attach instead of pasting text. Ctrl+V,
                // Cmd+V and Ctrl+Shift+V all count; plain text falls
                // through to the focused editor's own paste handler.
                let key = keystroke.key.as_str();
                let is_paste = ((key == "v" || key == "V")
                    && (keystroke.modifiers.control || keystroke.modifiers.platform)
                    && !keystroke.modifiers.alt)
                    || (key == "insert" && keystroke.modifiers.shift);
                if !is_paste {
                    return;
                }

                let Some(pane_id) = this.focused_pane_id else {
                    return;
                };
                // Full-screen programs (vim, less, htop) own the keyboard;
                // don't turn their Ctrl+V into an attachment.
                if this
                    .pane_ref(pane_id)
                    .is_some_and(|pane| pane.terminal.read(cx).is_alt_screen())
                {
                    return;
                }
                if this.attach_image_from_clipboard(pane_id, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .can_drop(|value, _, _| {
                value.downcast_ref::<ExternalPaths>().is_some_and(|paths| {
                    paths
                        .paths()
                        .iter()
                        .any(|path| attachments::looks_like_image(path))
                })
            })
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.drop_external_images(paths, window, cx);
            }))
            .when(self.sidebar_visible, |this| {
                this.child(
                    div()
                        .w(self.sidebar_width)
                        .h_full()
                        .flex_shrink_0()
                        .child(sidebar_content),
                )
            })
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .overflow_hidden()
                    .child(main_content),
            )
            .when(self.terminal_context_menu.is_some(), |this| {
                this.child(self.render_terminal_context_menu_layer(cx))
            })
            .when(self.history_search.is_some(), |this| {
                this.child(self.render_history_search_layer(cx))
            })
            .when(self.image_viewer.is_some(), |this| {
                this.child(self.render_image_viewer_layer(cx))
            });
        diagnostics::render_end(diag_render);
        root
    }
}

/// How many non-empty screen lines `T3_STATUS_DEBUG` prints per terminal.
const DEBUG_SCREEN_LINES: usize = 14;

/// Sidebar form of a session title: full text up to 160 chars, then an ellipsis.
/// Uses `char_indices().nth(160)` so short titles cost one pass that stops at
/// the end — unlike `chars().count()`, which always scans the whole string —
/// and is cached on the session so rendering never recomputes it.
fn truncate_title(title: &SharedString) -> SharedString {
    match title.as_str().char_indices().nth(160) {
        Some((idx, _)) => SharedString::from(format!("{}...", &title.as_str()[..idx])),
        None => title.clone(),
    }
}

/// Cheap, stable fingerprint of what is on screen, used to tell whether the
/// user has already looked at a finished/failed terminal.
fn screen_fingerprint(lines: &[String], title: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lines.hash(&mut hasher);
    title.hash(&mut hasher);
    hasher.finish()
}

/// How alerts and the Agents list name a pane: its agent app first, else a
/// meaningful title, else a neutral fallback.
fn pane_agent_name(pane: &TerminalPane) -> String {
    if let Some(app) = &pane.app {
        return app.to_string();
    }
    let title = pane.display_title.trim();
    if !title.is_empty() && !title.starts_with("terminal ") {
        return title.to_string();
    }
    "An agent".to_string()
}

/// Build a pane's input editor plus its change subscription.
///
/// The subscription defers its work: `InputEvent::Change` is emitted while the
/// input entity is leased, so ghost text and the completion menu are refreshed
/// after that effect cycle instead of re-entering the entity.
fn build_pane_input(
    pane_id: usize,
    window: &mut Window,
    cx: &mut Context<AppState>,
) -> (Entity<InputState>, gpui::Subscription) {
    let input = cx.new(|cx| {
        let mut state = InputState::new(window, cx)
            .placeholder("Run commands")
            .auto_grow(1, 8);
        // Shell syntax highlighting, matching the block command language.
        state.set_highlighter("bash", cx);
        state
    });
    let subscription = cx.subscribe_in(
        &input,
        window,
        move |_this: &mut AppState, _input, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Change) {
                let weak = cx.entity().downgrade();
                window.defer(cx, move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.on_pane_input_changed(pane_id, window, cx);
                    });
                });
            }
        },
    );
    (input, subscription)
}

/// Replace the input value and place the cursor at the end.
fn set_input_text(
    input: &Entity<InputState>,
    text: String,
    window: &mut Window,
    cx: &mut Context<AppState>,
) {
    let cursor = completion::utf16_len(&text);
    set_input_text_with_cursor(input, text, cursor, window, cx);
}

fn set_input_text_with_cursor(
    input: &Entity<InputState>,
    text: String,
    cursor_utf16: usize,
    window: &mut Window,
    cx: &mut Context<AppState>,
) {
    input.update(cx, |state, cx| {
        state.set_value(text, window, cx);
        let cursor = cursor_utf16.min(completion::utf16_len(state.value().as_str()));
        let position = state.text().offset_to_position(cursor);
        state.set_cursor_position(position, window, cx);
    });
}

fn move_input_cursor(
    input: &Entity<InputState>,
    cursor_utf16: usize,
    window: &mut Window,
    cx: &mut Context<AppState>,
) {
    input.update(cx, |state, cx| {
        let cursor = cursor_utf16.min(completion::utf16_len(state.value().as_str()));
        let position = state.text().offset_to_position(cursor);
        state.set_cursor_position(position, window, cx);
    });
}

/// Byte offset where the word before `cursor` starts (readline `Ctrl+W`).
fn previous_word_start(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    let mut start = cursor;
    while let Some(ch) = value[..start].chars().next_back() {
        if !ch.is_whitespace() {
            break;
        }
        start -= ch.len_utf8();
    }
    while let Some(ch) = value[..start].chars().next_back() {
        if ch.is_whitespace() {
            break;
        }
        start -= ch.len_utf8();
    }
    start
}

/// Byte offset just past the word at `cursor` (readline `Alt+F`).
fn next_word_end(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    let mut end = cursor;
    while let Some(ch) = value[end..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        end += ch.len_utf8();
    }
    while let Some(ch) = value[end..].chars().next() {
        if ch.is_whitespace() {
            break;
        }
        end += ch.len_utf8();
    }
    end
}

fn detect_shell() -> (String, String) {
    if cfg!(windows) {
        (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            "cmd".to_string(),
        )
    } else {
        if let Ok(shell) = std::env::var("SHELL") {
            let name = std::path::Path::new(&shell)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shell".to_string());
            return (shell, name);
        }
        ("/bin/bash".to_string(), "bash".to_string())
    }
}
#[cfg(target_os = "linux")]
fn ensure_linux_desktop_entry() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let apps_dir = data_dir.join("applications");
    let icons_512_dir = data_dir.join("icons/hicolor/512x512/apps");
    let _ = std::fs::create_dir_all(&apps_dir);
    let _ = std::fs::create_dir_all(&icons_512_dir);

    let icon_path = icons_512_dir.join("kathamo.png");
    let needs_icon_update = match std::fs::read(&icon_path) {
        Ok(existing) => existing != APP_ICON_PNG,
        Err(_) => true,
    };
    if needs_icon_update {
        let _ = std::fs::write(&icon_path, APP_ICON_PNG);
        if let Ok(img) = image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png)
        {
            for size in [256, 128, 64, 48, 32, 16] {
                let size_dir = data_dir.join(format!("icons/hicolor/{size}x{size}/apps"));
                let _ = std::fs::create_dir_all(&size_dir);
                let target = size_dir.join("kathamo.png");
                let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
                let _ = resized.save_with_format(&target, image::ImageFormat::Png);
            }
        }
    }

    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kathamo"));
    let exe_str = exe_path.to_string_lossy();

    let desktop_content = format!(
        "[Desktop Entry]\n\
        Type=Application\n\
        Name=Kathamo\n\
        Comment=GPU-accelerated terminal\n\
        Exec={exe_str}\n\
        Icon=kathamo\n\
        Terminal=false\n\
        Categories=System;TerminalEmulator;\n\
        StartupWMClass=kathamo\n"
    );

    let _ = std::fs::write(apps_dir.join("kathamo.desktop"), desktop_content);
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("report") => return cli::run_report(&args.collect::<Vec<_>>()),
        Some("skill") => return cli::run_skill(&args.collect::<Vec<_>>()),
        Some("integration") => return integrations::run(&args.collect::<Vec<_>>()),
        Some("status-hook") => return integrations::run_hook(&args.collect::<Vec<_>>()),
        Some("status-event") => return cli::run_status_event(),
        Some("status") => return cli::run_status(&args.collect::<Vec<_>>()),
        Some("pane") => {
            #[cfg(unix)]
            return kathamo_compat::run_cli(&args.collect::<Vec<_>>());
            #[cfg(not(unix))]
            anyhow::bail!("native reporting compatibility requires a Unix socket");
        }
        Some("--help" | "-h") => {
            println!(
                "Usage: kathamo [report|skill|integration|status|status-event|status-hook]\nNative compatibility: kathamo pane <report-agent|report-agent-session|release-agent> ..."
            );
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("kathamo {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(command) => anyhow::bail!("unsupported Kathamo command: {command}"),
        None => {}
    }
    gpui_platform::application()
        .with_assets(CombinedAssets)
        .run(|cx: &mut App| {
            #[cfg(target_os = "linux")]
            ensure_linux_desktop_entry();
            if let Err(error) = paths::ensure_dirs() {
                eprintln!("failed to create app data directories: {error}");
            }
            let app_settings = settings::load();
            let restore_enabled = app_settings.restore.sessions
                && std::env::var_os("KATHAMO_DISABLE_RESTORE").is_none();
            let load_history = app_settings.restore.agent_history && restore_enabled;
            let window_icon = image::load_from_memory(APP_ICON_PNG).ok().map(|image| Arc::new(image.into_rgba8()));
            gpui_component::init(cx);
            init_kathamo_input_bindings(cx);
            let initial_mode = app_settings.theme.resolve_mode(cx.window_appearance());
            gpui_component::Theme::change(initial_mode, None, cx);
            load_embedded_fonts(cx);
            sync_component_fonts(cx);

            std::thread::spawn(|| {
                completion::prewarm();
                let _ = shell_history::cached();
            });

            let palette = load_palette(app_settings.palette_scheme, initial_mode.is_dark());
            let persistence_path = Some(paths::workspace_path());

            let displays: Vec<Bounds<Pixels>> = cx
                .displays()
                .iter()
                .map(|display| display.visible_bounds())
                .collect();
            let bounds = settings::window_bounds(app_settings.window, &displays);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(bounds),
                    titlebar: Some(TitleBar::title_bar_options()),
                    window_decorations: Some(WindowDecorations::Client),
                    app_id: Some("kathamo".to_string()),
                    icon: window_icon,
                    ..Default::default()
                },
                |window, cx| {
                    let app_state = cx.new(|_| AppState {
                        projects: Vec::new(),
                        active_project_idx: 0,
                        next_id: 1,
                        session_index: HashMap::new(),
                        pane_index: HashMap::new(),
                        palette: palette.clone(),
                        focused_pane_id: None,
                        sidebar_visible: app_settings.sidebar.visible,
                        active_view: ActiveView::Workspace,
                        theme_preference: app_settings.theme,
                        palette_scheme: app_settings.palette_scheme,
                        default_agent_model: app_settings.default_agent_model.clone(),
                        fallback_agent_model: agent_auth::omp_default_model(),
                        integration_panel_visible: false,
                        integration_feedback: SharedString::from("Setup is opt-in. Status becomes Reporting only after a live event arrives."),
                        sidebar_width: px(app_settings.sidebar.width.max(120.0)),
                        drag_start_pos: None,
                        persistence_path: persistence_path.clone(),
                        save_generation: Arc::new(AtomicU64::new(0)),
                        startup_loaded: false,
                        quit_subscription: None,
                        window_active: true,
                        status_primed: false,
                        pending_alerts: Vec::new(),
                        attention_seq: 0,
                        project_menu: None,
                        renaming_project: None,
                        terminal_context_menu: None,
                        history_search: None,
                        image_viewer: None,
                        settings: app_settings.clone(),
                        dirty: false,
                        last_window_bounds: app_settings.window,
                        persistence_enabled: std::env::var_os("KATHAMO_DISABLE_RESTORE").is_none(),
                        clear_data_confirm: false,
                    });
                    app_state.update(cx, |state, cx| state.observe_appearance(window, cx));

                    // Close button / window-manager close: flush everything
                    // synchronously before the window goes away.
                    let close_state = app_state.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        if let Some(state) = close_state.upgrade() {
                            let _ = state.update(cx, |state, cx| {
                                state.save_all_sync(Some(window), cx);
                            });
                        }
                        true
                    });

                    let weak_state = app_state.downgrade();
                    let load_path = persistence_path.clone();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        let restored = cx
                            .background_executor()
                            .spawn(async move {
                                let Some(path) = load_path else {
                                    return RestoreData {
                                        workspace: workspace_state::SavedWorkspace::default(),
                                        scrollbacks: HashMap::new(),
                                        transcripts: HashMap::new(),
                                    };
                                };
                                let workspace = if restore_enabled {
                                    workspace_state::load(&path)
                                } else {
                                    workspace_state::SavedWorkspace::default()
                                };
                                let mut scrollbacks = HashMap::new();
                                let mut transcripts = HashMap::new();
                                let mut live_uids = HashSet::new();
                                for project in &workspace.projects {
                                    for session in &project.sessions {
                                        for pane in &session.panes {
                                            live_uids.insert(pane.uid.clone());
                                            if let Some(text) =
                                                workspace_state::read_scrollback(&pane.uid)
                                            {
                                                scrollbacks.insert(pane.uid.clone(), text);
                                            }
                                            if load_history {
                                                let keys = std::iter::once(None::<&agent_auth::AgentModel>)
                                                    .chain(pane
                                                        .chat
                                                        .models
                                                        .iter()
                                                        .map(|chat| chat.model.as_ref()));
                                                for model in keys {
                                                    let key = conversations::conversation_key(model);
                                                    let conversation_path = paths::conversations_dir()
                                                        .join(&pane.uid)
                                                        .join(format!("{key}.jsonl"));
                                                    let messages = conversations::load(&conversation_path);
                                                    if !messages.is_empty() {
                                                        transcripts.insert(
                                                            transcript_key(&pane.uid, &key),
                                                            messages,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                // Never prune from an empty/malformed workspace:
                                // that would delete scrollback for every pane.
                                if restore_enabled && !workspace.projects.is_empty() {
                                    workspace_state::prune_stale_scrollbacks(&live_uids);
                                }
                                RestoreData {
                                    workspace,
                                    scrollbacks,
                                    transcripts,
                                }
                            })
                            .await;
                        let _ = weak_state
                            .update(cx, |state, cx| state.apply_saved_workspace(restored, cx));
                    })
                    .detach();

                    app_state.update(cx, |state, cx| {
                        state.persistence_path = persistence_path.clone();
                        state.startup_loaded = false;
                        state.quit_subscription = Some(cx.on_app_quit(move |state, cx| {
                            state.save_all_sync(None, cx);
                            async {}
                        }));
                    });

                    // Periodic snapshot: bounds the amount of work lost if the
                    // process is killed instead of closed.
                    let autosave_state = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        loop {
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(30))
                                .await;
                            match autosave_state.update(cx, |state, cx| state.autosave(cx)) {
                                Ok(()) => {}
                                Err(_) => break,
                            }
                        }
                    })
                    .detach();

                    let agent_state_weak = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        let mut streaming = false;
                        loop {
                            let interval = if streaming { 16 } else { 50 };
                            cx.background_executor().timer(std::time::Duration::from_millis(interval)).await;
                            match agent_state_weak.update(cx, |state, cx| state.poll_agent_events(cx)) {
                                Ok(active) => streaming = active,
                                Err(_) => break,
                            }
                        }
                    }).detach();

                    let app_state_weak = app_state.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        loop {
                            cx.background_executor().timer(POLL_INTERVAL).await;

                            // Snapshot the shell pids on the UI thread (a borrow
                            // over the pane list), do the blocking `/proc`
                            // inspection on the background executor, then apply
                            // the result back on the UI thread. Nothing in this
                            // loop reads files or waits on I/O while a frame
                            // could be building.
                            let Ok(pids) = app_state_weak.update(cx, |state, _| state.pane_pids())
                            else {
                                break;
                            };
                            let foregrounds = cx
                                .background_executor()
                                .spawn(async move {
                                    pids.into_iter()
                                        .map(|pid| {
                                            (pid, status::foreground_process_names(Some(pid)))
                                        })
                                        .collect::<HashMap<u32, Arc<[String]>>>()
                                })
                                .await;

                            let res = app_state_weak.update(cx, |state, cx| {
                                state.poll_sessions_status_with(&foregrounds, cx);
                            });
                            if res.is_err() {
                                break;
                            }
                        }
                    })
                    .detach();

                    // Opt-in diagnostics: one line per second with frame
                    // timing, UI-thread work, CPU, RSS and wakeups. Not spawned
                    // at all unless `KATHAMO_DIAGNOSTICS` is set.
                    if diagnostics::enabled() {
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            let mut sampler = diagnostics::Sampler::new();
                            loop {
                                cx.background_executor()
                                    .timer(diagnostics::REPORT_INTERVAL)
                                    .await;
                                eprintln!("{}", sampler.report());
                            }
                        })
                        .detach();
                    }

                    // TEMPORARY DIAGNOSTIC: KATHAMO_RESIZE_PROBE=<rounds> drags
                    // the window edge in small steps, the way a real resize
                    // does, while the status poller keeps ticking.
                    if let Some(probe) = std::env::var_os("KATHAMO_RESIZE_PROBE").or_else(|| std::env::var_os("MARKUP_RESIZE_PROBE")) {
                        let rounds: usize = probe.to_string_lossy().parse().unwrap_or(10);
                        let handle = window.window_handle();
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            let (mut w, mut h) = (1200.0f32, 780.0f32);
                            for _ in 0..rounds {
                                for _ in 0..60 {
                                    w = (w - 12.0).max(360.0);
                                    h = (h - 8.0).max(240.0);
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(16))
                                        .await;
                                    if handle
                                        .update(cx, |_, window, _| {
                                            window.resize(size(px(w), px(h)));
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                for _ in 0..60 {
                                    w = (w + 12.0).min(1200.0);
                                    h = (h + 8.0).min(780.0);
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(16))
                                        .await;
                                    if handle
                                        .update(cx, |_, window, _| {
                                            window.resize(size(px(w), px(h)));
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            eprintln!("[probe] {rounds} drag rounds finished without a panic");
                            let _ = cx.update(|cx| cx.quit());
                        })
                        .detach();
                    }

                    cx.new(|cx| Root::new(app_state, window, cx))
                },
            )
            .expect("Failed to open window");

            cx.activate(true);
        });

    Ok(())
}

#[cfg(test)]
mod resize_tests {
    //! Window-resize regression tests.
    //!
    //! A resize is the one event that re-lays-out everything at once: the
    //! resizable sidebar panels, the terminal grid (cols/rows recomputed from
    //! the new bounds) and the client-drawn title bar. These tests drive the
    //! real view through the same window options `main` uses and draw a frame
    //! after every resize step.

    use super::*;
    use gpui::{HeadlessAppContext, Modifiers, VisualContext, VisualTestContext, point, size};
    use std::ops::{Deref, DerefMut};

    // VisualTestContext supplies input simulation, while HeadlessAppContext is
    // the public constructor that installs a real renderer. Point the visual
    // context at the headless app and its executors; keep the owner alive for
    // the duration of each test so it can shut the app down afterward.
    pub(crate) struct TestAppContext {
        visual: gpui::TestAppContext,
        _headless: HeadlessAppContext,
    }

    impl TestAppContext {
        pub(crate) fn new() -> Self {
            let text = gpui_platform::current_platform(true).text_system();
            assert!(
                !text.all_font_names().is_empty(),
                "headless visual tests require a real text system and installed fonts; enable gpui-platform's x11 or wayland feature on Linux"
            );
            let headless = HeadlessAppContext::with_platform(text.clone(), Arc::new(()), || {
                let renderer = gpui_platform::current_headless_renderer()?
                    .ok_or_else(|| anyhow::anyhow!("headless GPU renderer unavailable"))?;
                Ok(Some(renderer))
            });
            let mut visual = gpui::TestAppContext::build_with_text_system(
                gpui::TestDispatcher::new(0),
                None,
                text,
            );
            visual.app = headless.app.clone();
            visual.background_executor = headless.background_executor.clone();
            visual.foreground_executor = headless.foreground_executor.clone();
            Self {
                visual,
                _headless: headless,
            }
        }
    }

    impl Deref for TestAppContext {
        type Target = gpui::TestAppContext;

        fn deref(&self) -> &Self::Target {
            &self.visual
        }
    }

    impl DerefMut for TestAppContext {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.visual
        }
    }

    pub(crate) trait DrawFrame {
        fn draw_frame(&mut self) -> anyhow::Result<()>;
    }

    impl DrawFrame for VisualTestContext {
        fn draw_frame(&mut self) -> anyhow::Result<()> {
            self.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.render_to_image().map(drop)
            })
        }
    }

    /// One project with one live session, built exactly like `main` does.
    fn build_app_state(cx: &mut Context<AppState>) -> AppState {
        let palette = ColorPalette::default();
        let dir = std::env::current_dir().expect("current dir");
        let weak = cx.entity().downgrade();
        let (terminal, pid, generation, terminal_subscription) =
            AppState::spawn_terminal_view(Some(&dir), palette.clone(), weak, 1, None, cx)
                .expect("terminal view");
        let title = SharedString::from("terminal 1");

        let mut pane =
            TerminalPane::new(1, generation, terminal, title.clone(), title.clone(), pid);
        pane.terminal_subscription = Some(terminal_subscription);
        // Tests assert exact history indices; the developer machine's real
        // shell history must not leak into that.
        pane.shell_history = Arc::new(Vec::new());

        AppState {
            projects: vec![Project {
                id: 1,
                name: SharedString::from("kathamo"),
                path: dir,
                sessions: vec![Session {
                    id: 1,
                    uid: paths::uid("s"),
                    title: title.clone(),
                    display_title: title,
                    logo: logos::default_logo(),
                    pane_tree: Some(PaneNode::Leaf(1)),
                    maximized_pane_id: None,
                    panes: vec![pane],
                    status: SessionStatus::Idle,
                }],
                active_session_idx: 0,
                expanded: true,
            }],
            active_project_idx: 0,
            next_id: 2,
            session_index: HashMap::from([(1, (0, 0))]),
            pane_index: HashMap::from([(1, (0, 0, 0))]),
            palette,
            focused_pane_id: Some(1),
            sidebar_visible: true,
            active_view: ActiveView::Workspace,
            theme_preference: ThemePreference::System,
            palette_scheme: PaletteScheme::default(),
            default_agent_model: None,
            fallback_agent_model: None,
            integration_panel_visible: false,
            integration_feedback: SharedString::from(
                "Setup is opt-in. Status becomes Reporting only after a live event arrives.",
            ),
            sidebar_width: SIDEBAR_WIDTH,
            drag_start_pos: None,
            persistence_path: None,
            save_generation: Arc::new(AtomicU64::new(0)),
            startup_loaded: true,
            quit_subscription: None,
            window_active: true,
            status_primed: false,
            pending_alerts: Vec::new(),
            attention_seq: 0,
            project_menu: None,
            renaming_project: None,
            terminal_context_menu: None,
            history_search: None,
            image_viewer: None,
            settings: settings::AppSettings::default(),
            dirty: false,
            last_window_bounds: None,
            persistence_enabled: false,
            clear_data_confirm: false,
        }
    }

    /// Open the app the way `main` does and return a context that can draw it.
    pub(crate) fn open_app(cx: &mut TestAppContext) -> (VisualTestContext, Entity<AppState>) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init_kathamo_input_bindings(cx);
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
            load_embedded_fonts(cx);
            sync_component_fonts(cx);
        });

        let state = cx.new(build_app_state);
        let handle = cx
            .update(|cx: &mut App| {
                let bounds = Bounds::centered(None, size(px(1200.), px(780.)), cx);
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                        titlebar: Some(TitleBar::title_bar_options()),
                        window_decorations: Some(WindowDecorations::Client),
                        app_id: Some("kathamo".to_string()),
                        icon: image::load_from_memory(APP_ICON_PNG)
                            .ok()
                            .map(|image| Arc::new(image.into_rgba8())),
                        ..Default::default()
                    },
                    |window, cx| {
                        state.update(cx, |state, cx| state.observe_appearance(window, cx));
                        cx.new(|cx| Root::new(state.clone(), window, cx))
                    },
                )
            })
            .expect("open window");

        let cx = VisualTestContext::from_window(*handle.deref(), cx);
        cx.run_until_parked();
        (cx, state)
    }

    #[test]
    fn new_panes_adopt_the_fallback_model() {
        let mut cx = TestAppContext::new();
        let (mut cx, state) = open_app(&mut cx);
        let model = agent_auth::AgentModel {
            provider: "google-antigravity".into(),
            id: "gemini-default".into(),
            backend: agent_auth::ModelBackend::Omp,
        };
        cx.update_window_entity(&state, |s, window, cx| {
            s.fallback_agent_model = Some(model.clone());
            s.create_new_session_in_project(0, window, cx);
        });
        state.read_with(&cx, |s, _| {
            assert_eq!(s.effective_default_agent_model(), Some(&model));
            let pane = &s.projects[0].sessions.last().unwrap().panes[0];
            assert_eq!(pane.chat.selected_model.as_ref(), Some(&model));
        });
    }

    #[test]
    fn agent_default_screen_shows_for_an_empty_chat() {
        let mut cx = TestAppContext::new();
        let (mut cx, state) = open_app(&mut cx);
        let model = agent_auth::AgentModel {
            provider: "google-antigravity".into(),
            id: "gemini-default".into(),
            backend: agent_auth::ModelBackend::Omp,
        };
        let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
        cx.update_window_entity(&state, |s, window, cx| {
            s.set_chat_mode(pane_id, true, window, cx);
            // Set the model after entering agent mode so no bridge is warmed.
            s.pane_mut(pane_id).unwrap().chat.selected_model = Some(model.clone());
        });
        cx.draw_frame().expect("draw agent default screen");
        assert!(
            cx.debug_bounds("agent-default-screen").is_some(),
            "an empty agent pane shows the default screen"
        );
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(pane_id).unwrap();
            assert_eq!(pane.chat.selected_model.as_ref(), Some(&model));
            assert!(pane.chat.visible_messages().is_empty());
        });
    }

    #[test]
    fn startup_failures_become_actionable_default_screen_notes() {
        let (message, detail) = agent_startup_notice(
            "rpi process exited (exit status: 2): error: No API key. Set one of: models.json apiKey",
        );
        assert!(message.contains("/auth"));
        assert!(message.contains("/model"));
        assert!(detail.starts_with("rpi process exited"));

        let (message, _) = agent_startup_notice("rpi process exited (exit status: 127)");
        assert!(message.contains("could not start"));
    }

    #[test]
    fn window_resizes_without_panicking() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        let terminal = app_state.read_with(&cx, |state, _| {
            state.active_session().expect("a session").panes[0]
                .terminal
                .clone()
        });

        let mut grid_sizes = Vec::new();
        for window_size in [
            size(px(1100.), px(700.)),
            size(px(900.), px(600.)),
            size(px(700.), px(500.)),
            size(px(520.), px(400.)),
            size(px(380.), px(300.)),
            size(px(280.), px(220.)),
            size(px(200.), px(160.)),
            size(px(1200.), px(780.)),
        ] {
            cx.simulate_resize(window_size);
            cx.run_until_parked();
            cx.draw_frame().expect("a frame at the new size");

            // The grid has to track the window, and every scrape has to come
            // back with exactly the rows the grid has: a scrape that names rows
            // the grid resized away is what aborted the process on a drag.
            let (cols, rows) = terminal.read_with(&cx, |view, _| view.dimensions());
            assert!(cols >= 2 && rows >= 1, "grid collapsed to {cols}x{rows}");
            let scraped = terminal.read_with(&cx, |view, _| view.bottom_lines(usize::MAX));
            assert_eq!(scraped.len(), rows, "scrape out of step with the grid");
            grid_sizes.push((cols, rows));
        }

        // ...and the grid really did follow the window, rather than both sides
        // agreeing on a size nobody updated.
        let (big_cols, big_rows) = grid_sizes[0];
        let (small_cols, small_rows) = grid_sizes[6];
        assert!(
            small_rows < big_rows && small_cols < big_cols,
            "grid did not follow the window: {small_cols}x{small_rows} at 200x160, \
             {big_cols}x{big_rows} at 1100x700"
        );
    }

    /// The status poller runs between frames, so it can tick after the paint
    /// pass has already shrunk the terminal grid but before the next render
    /// syncs the cached row count. Scraping the screen in that window asks the
    /// grid for rows that no longer exist.
    #[test]
    fn status_poll_between_resize_frames_does_not_panic() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        // A frame at the large size: the grid is laid out big while the cached
        // row count still holds the size from the previous frame.
        cx.draw_frame().expect("a frame at the opening size");

        // Shrink the window and draw one frame: the render syncs the cached
        // dims to the large grid, then the paint shrinks the grid.
        cx.simulate_resize(size(px(560.), px(320.)));
        cx.run_until_parked();
        cx.draw_frame().expect("a frame at the smaller size");

        // The poller's ticks are what a user sees as a crash while dragging.
        app_state.update(&mut cx, |state, cx| state.poll_sessions_status(cx));
        app_state.update(&mut cx, |state, cx| state.poll_sessions_status(cx));
    }

    /// Simulating dragging across the fixed sidebar and window bounds must
    /// stay panic-free.
    #[test]
    fn dragging_the_sidebar_handle_does_not_panic() {
        let mut cx = TestAppContext::new();
        let (mut cx, _) = open_app(&mut cx);
        cx.draw_frame().expect("a frame at the opening size");

        let y = px(400.0);
        let grab = point(px(327.5), y);
        cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
        for dx in [-400.0, -50.0, 0.0, 60.0, 200.0, 400.0, -800.0, 30.0] {
            cx.simulate_mouse_move(
                point(px(327.5 + dx), y),
                Some(MouseButton::Left),
                Modifiers::none(),
            );
            cx.draw_frame().expect("frame while dragging sidebar");
        }
        cx.simulate_mouse_up(grab, MouseButton::Left, Modifiers::none());
        cx.draw_frame().expect("frame after dragging sidebar");
    }

    #[test]
    fn sidebar_toggle_and_resize_without_panicking() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        for visible in [false, true, false, true] {
            app_state.update(&mut cx, |state, cx| {
                state.sidebar_visible = visible;
                cx.notify();
            });

            cx.simulate_resize(size(px(640.), px(480.)));
            cx.run_until_parked();
            cx.draw_frame().expect("a frame after toggling");
            cx.simulate_resize(size(px(1200.), px(780.)));
            cx.run_until_parked();
            cx.draw_frame().expect("a frame after toggling");
        }
    }

    #[test]
    fn fixed_sidebar_width_stays_constant_on_resize() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        for new_size in [
            size(px(640.), px(480.)),
            size(px(1440.), px(900.)),
            size(px(1920.), px(1080.)),
        ] {
            cx.simulate_resize(new_size);
            cx.run_until_parked();
            cx.draw_frame().expect("drawn frame");

            let width = app_state.read_with(&cx, |state, _| state.sidebar_width);
            assert_eq!(
                width, SIDEBAR_WIDTH,
                "sidebar width must remain fixed across window resizes"
            );
        }
    }
}

#[cfg(test)]
mod xtgettcap_tests {
    //! End-to-end check of the terminal crate's XTGETTCAP passthrough: a
    //! capability query must produce a reply on the PTY writer and must never
    //! reach the grid as printable output.

    use alacritty_terminal::event::WindowSize;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use terminal::{ColorPalette, GpuiEventProxy, PtyWriter, TerminalNotifier, TerminalState};

    /// Writer sink that records everything the terminal writes back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A terminal with a real PTY writer installed, like the app has.
    fn terminal_with_capture() -> (TerminalState, Capture) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let proxy = GpuiEventProxy::new(tx);
        // The notifier slot is shared with the proxy/Term, so installing the
        // writer after construction is exactly what `TerminalView` does.
        let slot = proxy.notifier_slot();
        let state = TerminalState::new(80, 24, proxy);

        let capture = Capture::default();
        let writer: PtyWriter = Arc::new(Mutex::new(
            Box::new(capture.clone()) as Box<dyn std::io::Write + Send>
        ));
        *slot.lock() = Some(TerminalNotifier::new(
            writer,
            Arc::new(Mutex::new(WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            })),
            Arc::new(Mutex::new(ColorPalette::default())),
        ));

        (state, capture)
    }

    #[test]
    fn rgb_capability_is_answered_with_a_value() {
        let (mut state, capture) = terminal_with_capture();

        // "RGB" hex-encoded is `524742`.
        state.process_bytes(b"\x1bP+q524742\x1b\\");

        let reply = String::from_utf8_lossy(&capture.0.lock()).to_string();
        // `1` hex-encoded is `31`.
        assert_eq!(reply, "\x1bP1+r524742=31\x1b\\");
    }

    #[test]
    fn unknown_capability_gets_the_invalid_reply() {
        let (mut state, capture) = terminal_with_capture();

        // "XYZ" hex-encoded is `58595a`.
        state.process_bytes(b"\x1bP+q58595a\x1b\\");

        let reply = String::from_utf8_lossy(&capture.0.lock()).to_string();
        assert_eq!(reply, "\x1bP0+r58595a\x1b\\");
    }

    #[test]
    fn multiple_capabilities_get_one_reply_each() {
        let (mut state, capture) = terminal_with_capture();

        // "RGB;XYZ" -> `524742;58595a`
        state.process_bytes(b"\x1bP+q524742;58595a\x1b\\");

        let reply = String::from_utf8_lossy(&capture.0.lock()).to_string();
        assert_eq!(reply, "\x1bP1+r524742=31\x1b\\\x1bP0+r58595a\x1b\\");
    }

    #[test]
    fn query_is_not_printed_to_the_grid() {
        let (mut state, capture) = terminal_with_capture();

        state.process_bytes(b"before \x1bP+q524742\x1b\\ after\r\n");

        // Content starts on the top row, so read the whole visible screen.
        let screen = state.bottom_lines(usize::MAX).join("\n");
        assert!(
            screen.contains("before  after"),
            "query text leaked into the grid: {screen:?}"
        );
        assert!(
            !capture.0.lock().is_empty(),
            "the query should have been answered"
        );
    }
}

#[cfg(test)]
mod pane_tests {
    use super::*;
    use gpui::{Modifiers, VisualContext, point};
    use resize_tests::{DrawFrame, TestAppContext, open_app};
    #[test]
    fn pane_node_split_and_contains() {
        let mut tree = PaneNode::Leaf(1);
        assert!(tree.contains(1));
        assert!(!tree.contains(2));
        assert_eq!(tree.first_leaf(), 1);

        assert!(tree.split(1, 2, 10, gpui::Axis::Horizontal));
        assert!(tree.contains(1));
        assert!(tree.contains(2));
        assert!(!tree.contains(3));

        assert!(tree.split(2, 3, 11, gpui::Axis::Vertical));
        assert!(tree.contains(3));
        assert_eq!(tree.first_leaf(), 1);
    }

    #[test]
    fn pane_node_remove_collapses_tree() {
        let mut tree = PaneNode::Leaf(1);
        tree.split(1, 2, 10, gpui::Axis::Horizontal);
        tree.split(2, 3, 11, gpui::Axis::Vertical);

        // Remove 2: the vertical split (2, 3) should collapse to 3
        let tree = tree.remove(2).expect("tree remains");
        assert!(tree.contains(1));
        assert!(!tree.contains(2));
        assert!(tree.contains(3));

        // Remove 3: the horizontal split (1, 3) should collapse to 1
        let tree = tree.remove(3).expect("tree remains");
        assert_eq!(tree, PaneNode::Leaf(1));

        // Remove 1: tree becomes None
        assert!(tree.remove(1).is_none());
    }

    #[test]
    fn visual_split_maximize_close_flow() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        // Split vertical
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Horizontal, window, cx);
        });
        cx.draw_frame().expect("frame after vertical split");

        let pane_count = app_state.read_with(&cx, |state, _| {
            state.active_project().unwrap().sessions[0].panes.len()
        });
        assert_eq!(pane_count, 2);

        let new_pane_id = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());

        // Toggle maximize
        app_state.update(&mut cx, |state, cx| {
            state.toggle_maximize_pane(new_pane_id, cx);
        });
        cx.draw_frame().expect("frame while maximized");

        let is_max = app_state.read_with(&cx, |state, _| {
            state.active_project().unwrap().sessions[0].maximized_pane_id
        });
        assert_eq!(is_max, Some(new_pane_id));

        // Restore
        app_state.update(&mut cx, |state, cx| {
            state.toggle_maximize_pane(new_pane_id, cx);
        });
        cx.draw_frame().expect("frame restored");

        // Close the new pane
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.close_pane(new_pane_id, window, cx);
        });
        cx.draw_frame().expect("frame after close");

        let pane_count_after = app_state.read_with(&cx, |state, _| {
            state.active_project().unwrap().sessions[0].panes.len()
        });
        assert_eq!(pane_count_after, 1);
    }

    #[test]
    fn dragging_split_terminal_divider_resizes_without_panic() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        // Split vertical
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Horizontal, window, cx);
        });
        cx.draw_frame().expect("frame after vertical split");

        let y = px(400.0);
        let grab = point(px(700.0), y);
        cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
        for dx in [-150.0, -50.0, 0.0, 50.0, 150.0] {
            cx.simulate_mouse_move(
                point(px(700.0 + dx), y),
                Some(MouseButton::Left),
                Modifiers::none(),
            );
            cx.draw_frame().expect("frame while dragging divider");
        }
        cx.simulate_mouse_up(grab, MouseButton::Left, Modifiers::none());
        cx.draw_frame().expect("frame after dragging divider");
    }

    #[test]
    fn reproduce_nested_split_bounds() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        // Split 1 vertically: Left: 1, Right: new (2)
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Horizontal, window, cx);
        });
        cx.draw_frame().expect("frame after vertical split");

        // Split 1 horizontally: Top-left: 1, Bottom-left: new (3)
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Vertical, window, cx);
        });
        cx.run_until_parked();
        cx.draw_frame().expect("frame after horizontal split");

        // Check terminal dimensions/bounds for all 3 panes
        let (term1, term2, term3) = app_state.read_with(&cx, |state, cx| {
            let session = &state.active_project().unwrap().sessions[0];
            let t1 = session.panes[0].terminal.read(cx).dimensions();
            let t2 = session.panes[1].terminal.read(cx).dimensions();
            let t3 = session.panes[2].terminal.read(cx).dimensions();
            (t1, t2, t3)
        });

        // Ensure no terminal collapsed to height 0 / 1 row
        assert!(term1.1 > 5, "term1 collapsed: {:?}", term1);
        assert!(term2.1 > 5, "term2 collapsed: {:?}", term2);
        assert!(term3.1 > 5, "term3 collapsed: {:?}", term3);
    }

    #[test]
    fn horizontal_then_vertical_nested_split_does_not_collapse() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        // Split 1 horizontally: Top: 1, Bottom: 2
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Vertical, window, cx);
        });
        cx.draw_frame().expect("frame after horizontal split");

        let bottom_id = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());

        // Split bottom vertically: Bottom-left: 2, Bottom-right: 3
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(bottom_id, gpui::Axis::Horizontal, window, cx);
        });
        cx.run_until_parked();
        cx.draw_frame().expect("frame after vertical split");

        let (term1, term2, term3) = app_state.read_with(&cx, |state, cx| {
            let session = &state.active_project().unwrap().sessions[0];
            let t1 = session.panes[0].terminal.read(cx).dimensions();
            let t2 = session.panes[1].terminal.read(cx).dimensions();
            let t3 = session.panes[2].terminal.read(cx).dimensions();
            (t1, t2, t3)
        });

        assert!(term1.1 > 5, "term1 collapsed: {:?}", term1);
        assert!(term2.1 > 5, "term2 collapsed: {:?}", term2);
        assert!(term3.1 > 5, "term3 collapsed: {:?}", term3);
    }

    #[test]
    fn session_isolation_switch_tabs_keeps_screens_separate() {
        // The Kathamo-style isolation contract: creating a second session must
        // not touch the first session's pane tree, and each tab renders its
        // own screen.
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Horizontal, window, cx);
        });
        cx.draw_frame().expect("frame after split in session 1");

        let new_pane_id = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());

        cx.update_window_entity(&app_state, |state, window, cx| {
            state.create_new_session_for_active_project(window, cx);
        });
        cx.draw_frame().expect("frame after adding a session");

        let (first_pane_count, second_tree) = app_state.read_with(&cx, |state, _| {
            let project = state.active_project().unwrap();
            (
                project.sessions[0].panes.len(),
                project.sessions[1].pane_tree.clone(),
            )
        });
        assert_eq!(first_pane_count, 2);
        // The new session starts on its own full-size screen with one pane.
        assert!(matches!(second_tree, Some(PaneNode::Leaf(_))));

        // Switch back to the first tab: the split is still there.
        app_state.update(&mut cx, |state, cx| {
            state.active_project_mut().unwrap().active_session_idx = 0;
            cx.notify();
        });
        cx.draw_frame().expect("frame after switching tabs");
        let first_tree = app_state.read_with(&cx, |state, _| {
            state.active_project().unwrap().sessions[0]
                .pane_tree
                .clone()
        });
        assert!(first_tree.map_or(false, |t| t.contains(1) && t.contains(new_pane_id)));
    }
    /// A project directory that is not the repository the test runs in, so
    /// opening it really is "a second project".
    fn scratch_project_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).expect("scratch project dir");
        dir
    }

    #[test]
    fn opening_a_second_project_keeps_the_first_one_alive() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        let dir = scratch_project_dir("kathamo-multi-project-open");
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.open_project_path(dir, window, cx);
        });
        cx.draw_frame().expect("frame with two projects");

        let (project_count, active_idx, first_sessions, second_sessions, expanded) = app_state
            .read_with(&cx, |state, _| {
                (
                    state.projects.len(),
                    state.active_project_idx,
                    state.projects[0].sessions.len(),
                    state.projects[1].sessions.len(),
                    state.projects[1].expanded,
                )
            });
        assert_eq!(project_count, 2);
        assert_eq!(active_idx, 1);
        assert_eq!(first_sessions, 1, "the first project keeps its session");
        assert_eq!(second_sessions, 1, "the new project starts with a session");
        assert!(expanded, "the newly opened project is expanded");
    }

    #[test]
    fn sessions_are_created_in_the_project_that_asks_for_them() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        let dir = scratch_project_dir("kathamo-multi-project-sessions");
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.open_project_path(dir, window, cx);
        });
        cx.draw_frame().expect("frame with two projects");

        // Create a session in the *first* project while the second one is the
        // active project: the new terminal must land in project 0 and switch
        // the workspace over to it.
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.create_new_session_in_project(0, window, cx);
        });
        cx.draw_frame().expect("frame after creating a session");

        let (first, second, active) = app_state.read_with(&cx, |state, _| {
            (
                state.projects[0].sessions.len(),
                state.projects[1].sessions.len(),
                state.active_project_idx,
            )
        });
        assert_eq!(first, 2);
        assert_eq!(second, 1);
        assert_eq!(active, 0);
    }

    #[test]
    fn closing_a_session_or_project_leaves_the_others_intact() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        let dir = scratch_project_dir("kathamo-multi-project-close");
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.open_project_path(dir, window, cx);
        });
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.create_new_session_in_project(0, window, cx);
        });
        cx.draw_frame()
            .expect("frame with two projects and two sessions");

        // Close the extra session in project 0. Project 1 must be untouched.
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.close_session(0, 1, window, cx);
        });
        cx.draw_frame().expect("frame after closing a session");
        let (first, second) = app_state.read_with(&cx, |state, _| {
            (
                state.projects[0].sessions.len(),
                state.projects[1].sessions.len(),
            )
        });
        assert_eq!((first, second), (1, 1));

        // Close the first project. The second one becomes active with its own
        // session still running.
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.close_project(0, window, cx);
        });
        cx.draw_frame().expect("frame after closing a project");
        let (project_count, active_sessions, name) = app_state.read_with(&cx, |state, _| {
            (
                state.projects.len(),
                state.active_session().map(|s| s.panes.len()).unwrap_or(0),
                state.projects[0].name.to_string(),
            )
        });
        assert_eq!(project_count, 1);
        assert_eq!(active_sessions, 1);
        assert_eq!(name, "kathamo-multi-project-close");
    }

    #[test]
    fn sidebar_renders_collapsed_and_expanded_projects() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        let dir = scratch_project_dir("kathamo-multi-project-collapse");
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.open_project_path(dir, window, cx);
        });
        cx.draw_frame().expect("frame with two expanded projects");

        // Collapse the first project while the second stays expanded.
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.toggle_project_expanded(0, cx);
        });
        cx.draw_frame().expect("frame with one collapsed project");
        let expanded = app_state.read_with(&cx, |state, _| {
            (state.projects[0].expanded, state.projects[1].expanded)
        });
        assert_eq!(expanded, (false, true));

        // Activating a collapsed project expands it again.
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.activate_project(0, window, cx);
        });
        cx.draw_frame().expect("frame after activating a project");
        let (active, expanded) = app_state.read_with(&cx, |state, _| {
            (state.active_project_idx, state.projects[0].expanded)
        });
        assert_eq!(active, 0);
        assert!(expanded);
    }

    #[test]
    fn sidebar_renders_with_agent_and_worker() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        cx.update_window_entity(&app_state, |state, _window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.is_agent = true;
            pane.app = Some(SharedString::from("claude"));
            pane.status = SessionStatus::Done;
            pane.attention.seen = false;
            pane.attention.last = Some(SessionStatus::Done);
            pane.attention.since = 42;
            pane.sub_agents.insert(
                "child".into(),
                attention::SubAgent {
                    id: "child".into(),
                    title: Some("explore".into()),
                    status: SessionStatus::Working,
                    since: 41,
                    seq: 41,
                },
            );
            cx.notify();
        });
        // Drawing a frame with agent and sub-agent panes succeeds without panic.
        cx.draw_frame().expect("frame with agent pane");
        let agents = app_state.read_with(&cx, |state, _| {
            state
                .projects
                .iter()
                .flat_map(|project| &project.sessions)
                .flat_map(|session| &session.panes)
                .filter(|pane| pane.is_agent)
                .count()
        });
        assert_eq!(agents, 1);
    }

    #[test]
    fn sidebar_new_session_and_remove_session_icons_align() {
        let mut cx = TestAppContext::new();
        let (mut cx, _) = open_app(&mut cx);
        cx.draw_frame()
            .expect("initial frame with project and session");

        let new_session_bounds = cx
            .debug_bounds("project-new-session")
            .expect("project-new-session button bounds");
        let remove_session_bounds = cx
            .debug_bounds("session-close")
            .expect("session-close button bounds");
        let new_session_right = new_session_bounds.origin.x + new_session_bounds.size.width;
        let remove_session_right =
            remove_session_bounds.origin.x + remove_session_bounds.size.width;
        assert_eq!(
            new_session_right, remove_session_right,
            "create new session icon and remove session icon must have the same right alignment"
        );
        assert_eq!(
            new_session_bounds.origin.x, remove_session_bounds.origin.x,
            "create new session icon and remove session icon must have the same horizontal start"
        );
        assert_eq!(
            new_session_bounds.size.width, remove_session_bounds.size.width,
            "create new session icon and remove session icon must have the same width"
        );
        assert_eq!(new_session_bounds.size.width, px(20.0));
        assert_eq!(new_session_right, px(249.0));
        assert_eq!(remove_session_right, px(249.0));
        assert_eq!(new_session_bounds.origin.x, px(229.0));
        assert_eq!(remove_session_bounds.origin.x, px(229.0));
    }

    #[test]
    fn project_rename_flow() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("initial frame");

        let project_id = app_state.read_with(&cx, |state, _| state.projects[0].id);

        // Start rename
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.start_renaming_project(project_id, window, cx);
        });
        assert!(app_state.read_with(&cx, |state, _| state.renaming_project.is_some()));

        // Frame should render the inline input cleanly
        cx.draw_frame().expect("frame while renaming project");

        // Finish rename
        cx.update_window_entity(&app_state, |state, _, cx| {
            state.finish_renaming_project(project_id, SharedString::from("renamed-project"), cx);
        });

        let (name, is_renaming) = app_state.read_with(&cx, |state, _| {
            (
                state.projects[0].name.clone(),
                state.renaming_project.is_some(),
            )
        });
        assert_eq!(name, "renamed-project");
        assert!(!is_renaming);

        // Frame renders with new project name
        cx.draw_frame().expect("frame with renamed project");
    }

    #[test]
    fn project_menu_and_remove_flow() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        let dir = scratch_project_dir("kathamo-menu-remove");
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.open_project_path(dir, window, cx);
        });
        cx.draw_frame().expect("frame with two projects");

        let (count, project_id) =
            app_state.read_with(&cx, |state, _| (state.projects.len(), state.projects[1].id));
        assert_eq!(count, 2);

        // Open 3-dot dropdown menu
        cx.update_window_entity(&app_state, |state, _, cx| {
            state.project_menu = Some(project_id);
            cx.notify();
        });
        // Frame renders menu popover and backdrop
        cx.draw_frame().expect("frame with project menu open");

        // Close/Remove the project
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.close_project(1, window, cx);
        });
        cx.draw_frame().expect("frame after removing project");

        let (final_count, menu) =
            app_state.read_with(&cx, |state, _| (state.projects.len(), state.project_menu));
        assert_eq!(final_count, 1);
        assert_eq!(menu, None);
    }
}

#[cfg(test)]
mod settings_and_theme_tests {
    use super::*;
    use gpui::{KeyDownEvent, Modifiers, VisualContext, point};
    use resize_tests::{DrawFrame, TestAppContext, open_app};

    #[test]
    fn settings_close_button_returns_to_workspace() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.active_view = ActiveView::Settings;
            cx.notify();
        });
        cx.draw_frame().expect("draw settings page frame");

        let bounds = cx
            .debug_bounds("settings-close-btn")
            .expect("settings close button bounds");
        let center = point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_mouse_move(center, None, Modifiers::default());
        cx.simulate_click(center, Modifiers::default());

        assert_eq!(
            app_state.read_with(&cx, |state, _| state.active_view),
            ActiveView::Workspace,
            "clicking the top-right close button must leave Settings"
        );
    }

    #[test]
    fn settings_page_toggle_and_render() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        // Initially on workspace
        let initial_view = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(initial_view, ActiveView::Workspace);

        // Switch to settings
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.active_view = ActiveView::Settings;
            cx.notify();
        });
        cx.draw_frame().expect("draw settings page frame");
        let in_settings = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(in_settings, ActiveView::Settings);

        // Switch back to workspace
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.active_view = ActiveView::Workspace;
            cx.notify();
        });
        cx.draw_frame().expect("draw workspace page frame");
        let back_to_workspace = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(back_to_workspace, ActiveView::Workspace);
    }

    #[test]
    fn settings_keyboard_shortcuts() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        // Ctrl+, opens settings
        cx.update_window_entity(&app_state, |state, window, cx| {
            let ev = KeyDownEvent {
                keystroke: gpui::Keystroke {
                    modifiers: Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    key: ",".to_string(),
                    key_char: Some(",".to_string()),
                },
                is_held: false,
                prefer_character_input: false,
            };
            state.on_key_down(&ev, window, cx);
        });
        let view = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(view, ActiveView::Settings);

        // Escape closes settings
        cx.update_window_entity(&app_state, |state, window, cx| {
            let ev = KeyDownEvent {
                keystroke: gpui::Keystroke {
                    modifiers: Modifiers::default(),
                    key: "escape".to_string(),
                    key_char: None,
                },
                is_held: false,
                prefer_character_input: false,
            };
            state.on_key_down(&ev, window, cx);
        });
        let view = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(view, ActiveView::Workspace);
    }

    #[test]
    fn terminal_pixels_follow_theme_after_returning_from_settings() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Horizontal, window, cx);
        });
        for (preference, light) in [
            (ThemePreference::Dark, false),
            (ThemePreference::Light, true),
            (ThemePreference::Dark, false),
            (ThemePreference::System, true),
        ] {
            cx.update_window_entity(&app_state, |state, window, cx| {
                state.active_view = ActiveView::Settings;
                state.set_theme_preference(preference, Some(window), cx);
            });
            cx.draw_frame().expect("draw settings");
            cx.update_window_entity(&app_state, |state, _, cx| {
                state.active_view = ActiveView::Workspace;
                cx.notify();
            });
            cx.draw_frame().expect("draw terminal");
            let bounds = app_state.read_with(&cx, |state, cx| {
                state.projects[0].sessions[0]
                    .panes
                    .iter()
                    .map(|pane| pane.terminal.read(cx).last_bounds())
                    .collect::<Vec<_>>()
            });
            assert_eq!(bounds.len(), 2);
            let pixels = cx.update(|window, _| {
                let image = window.render_to_image().expect("capture terminal");
                let scale = window.scale_factor();
                bounds
                    .iter()
                    .map(|bounds| {
                        let x = ((f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0)
                            * scale) as u32;
                        let y = ((f32::from(bounds.origin.y) + f32::from(bounds.size.height)
                            - 12.0)
                            * scale) as u32;
                        *image.get_pixel(x, y)
                    })
                    .collect::<Vec<_>>()
            });
            for pixel in pixels {
                if light {
                    assert!(
                        pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200,
                        "light terminal pixel: {pixel:?}"
                    );
                } else {
                    assert!(
                        pixel[0] < 60 && pixel[1] < 60 && pixel[2] < 60,
                        "dark terminal pixel: {pixel:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn session_or_pane_selection_resets_settings_view() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        // In settings, selecting session switches to workspace
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.active_view = ActiveView::Settings;
            state.select_session(0, 0, window, cx);
        });
        assert_eq!(
            app_state.read_with(&cx, |state, _| state.active_view),
            ActiveView::Workspace
        );

        // In settings, focusing pane switches to workspace
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.active_view = ActiveView::Settings;
            state.focus_pane(1, window, cx);
        });
        assert_eq!(
            app_state.read_with(&cx, |state, _| state.active_view),
            ActiveView::Workspace
        );
    }
}

#[cfg(test)]
mod kathamo_bottom_input_tests {
    use super::*;
    use gpui::{VisualContext, point};
    use resize_tests::{DrawFrame, TestAppContext, open_app};

    #[test]
    fn input_mode_pills_show_label_only_on_the_active_tab() {
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
        cx.draw_frame().expect("draw terminal mode");

        let terminal_terminal_mode = cx
            .debug_bounds("input-mode-terminal-pill")
            .expect("terminal pill renders");
        let agent_terminal_mode = cx
            .debug_bounds("input-mode-agent-pill")
            .expect("agent pill renders");
        assert!(
            terminal_terminal_mode.size.width > px(60.0),
            "the active terminal pill shows its icon and label"
        );
        assert!(
            agent_terminal_mode.size.width < px(40.0),
            "the inactive agent pill is icon only"
        );

        cx.update_window_entity(&state, |s, window, cx| {
            s.set_chat_mode(pane_id, true, window, cx);
        });
        cx.draw_frame().expect("draw agent mode");

        let terminal_agent_mode = cx
            .debug_bounds("input-mode-terminal-pill")
            .expect("terminal pill renders");
        let agent_agent_mode = cx
            .debug_bounds("input-mode-agent-pill")
            .expect("agent pill renders");
        assert!(
            terminal_agent_mode.size.width < px(40.0),
            "the inactive terminal pill is icon only"
        );
        assert!(
            agent_agent_mode.size.width > px(60.0),
            "the active agent pill shows its icon and label"
        );
        assert!(
            terminal_terminal_mode.size.width > terminal_agent_mode.size.width,
            "the terminal label appears only while terminal mode is active"
        );
        assert!(
            agent_agent_mode.size.width > agent_terminal_mode.size.width,
            "the agent label appears only while agent mode is active"
        );
    }

    #[test]
    fn streamed_thinking_and_answers_keep_their_boundaries() {
        use agent_bridge::AgentEvent;
        let mut messages = Vec::new();
        push_chat_event(&mut messages, AgentEvent::Thinking("Checking ".into()));
        push_chat_event(&mut messages, AgentEvent::Thinking("files".into()));
        push_chat_event(&mut messages, AgentEvent::Text("Found ".into()));
        push_chat_event(&mut messages, AgentEvent::Text("the issue".into()));
        push_chat_event(&mut messages, AgentEvent::Tool("read finished".into()));
        push_chat_event(&mut messages, AgentEvent::Text("Fixed it".into()));
        assert_eq!(messages.len(), 4);
        assert!(messages[0].role == ChatRole::Thinking && messages[0].text == "Checking files");
        assert!(messages[1].role == ChatRole::Assistant && messages[1].text == "Found the issue");
        assert!(messages[2].role == ChatRole::Tool && messages[2].text == "read finished");
        assert!(messages[3].role == ChatRole::Assistant && messages[3].text == "Fixed it");
    }

    #[test]
    fn streaming_split_keeps_fenced_blocks_whole() {
        let text = "intro\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\ntail";
        let (settled, tail) = split_streaming_markdown(text);
        assert_eq!(
            settled.trim_end(),
            "intro\n\n```rust\nfn a() {}\n\nfn b() {}\n```"
        );
        assert_eq!(tail, "tail");

        let (settled, tail) = split_streaming_markdown("first\n\nsecond");
        assert_eq!(settled.trim_end(), "first");
        assert_eq!(tail, "second");

        // A list still being written stays whole so numbering/nesting keep context.
        let (settled, tail) = split_streaming_markdown("Intro\n\n- one\n- two\n- three");
        assert_eq!(settled.trim_end(), "Intro");
        assert_eq!(tail, "- one\n- two\n- three");
    }

    #[test]
    fn text_events_stream_until_the_next_boundary() {
        use agent_bridge::AgentEvent;
        let mut messages = Vec::new();
        push_chat_event(&mut messages, AgentEvent::Text("Work".into()));
        assert!(messages[0].streaming);
        push_chat_event(&mut messages, AgentEvent::Tool("read".into()));
        assert!(!messages[0].streaming);
        push_chat_event(&mut messages, AgentEvent::Text("Done".into()));
        assert!(messages[2].streaming);
        finish_chat_streaming(&mut messages);
        assert!(!messages[2].streaming);
    }

    #[test]
    fn concurrent_tool_snapshots_stay_with_their_original_calls() {
        use agent_bridge::AgentEvent;
        let mut messages = Vec::new();
        for id in ["a", "b"] {
            push_chat_event(
                &mut messages,
                AgentEvent::ToolStart {
                    id: Some(id.into()),
                    name: "read".into(),
                    input: format!("input {id}"),
                },
            );
        }
        push_chat_event(&mut messages, AgentEvent::Thinking("Checking".into()));
        push_chat_event(
            &mut messages,
            AgentEvent::ToolUpdate {
                id: Some("b".into()),
                name: "read".into(),
                output: "partial".into(),
            },
        );
        push_chat_event(
            &mut messages,
            AgentEvent::ToolEnd {
                id: Some("a".into()),
                name: "read".into(),
                output: "result a".into(),
                failed: false,
            },
        );
        push_chat_event(
            &mut messages,
            AgentEvent::ToolEnd {
                id: Some("b".into()),
                name: "read".into(),
                output: "result b".into(),
                failed: true,
            },
        );
        assert_eq!(messages.len(), 3);
        let a = messages[0].tool.as_ref().unwrap();
        let b = messages[1].tool.as_ref().unwrap();
        assert!(a.input == "input a" && a.output == "result a" && a.state == ToolState::Completed);
        assert!(b.input == "input b" && b.output == "result b" && b.state == ToolState::Failed);
        assert!(messages[2].role == ChatRole::Thinking);
    }

    #[test]
    fn agent_fast_mode_and_speed_control() {
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
        cx.update_window_entity(&state, |s, window, cx| {
            s.set_chat_mode(id, true, window, cx);
        });
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            assert!(
                pane.chat.fast_mode,
                "fast mode should default to true for interactive speed"
            );
        });

        // Execute /fast slash command
        cx.update_window_entity(&state, |s, window, cx| {
            let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
            input.update(cx, |input, cx| input.set_value("/fast", window, cx));
            s.execute_pane_input(id, window, cx);
        });
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            assert!(
                !pane.chat.fast_mode,
                "toggling via /fast should switch to deep mode"
            );
            let last_msg = pane.chat.visible_messages().last().unwrap();
            assert_eq!(last_msg.role, ChatRole::Status);
            assert!(last_msg.text.contains("Deep mode"));
        });

        // Execute /thinking low slash command
        cx.update_window_entity(&state, |s, window, cx| {
            let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
            input.update(cx, |input, cx| input.set_value("/thinking low", window, cx));
            s.execute_pane_input(id, window, cx);
        });
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            assert!(
                pane.chat.fast_mode,
                "/thinking low should activate fast mode"
            );
            let last_msg = pane.chat.visible_messages().last().unwrap();
            assert_eq!(last_msg.role, ChatRole::Status);
            assert!(last_msg.text.contains("low"));
        });
    }

    #[test]
    fn agent_dynamic_autoscroll_and_user_lock() {
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
        cx.update_window_entity(&state, |s, window, cx| {
            s.set_theme_preference(ThemePreference::Dark, Some(window), cx);
            s.set_chat_mode(id, true, window, cx);
            let pane = s.pane_mut(id).unwrap();
            assert!(pane.chat.autoscroll, "autoscroll must be true by default");
            // Add several messages so the container has scrollable content
            for i in 0..15 {
                pane.chat.messages.push(ChatMessage::new(
                    if i % 2 == 0 {
                        ChatRole::User
                    } else {
                        ChatRole::Assistant
                    },
                    format!(
                        "Message {i} with several lines of text to ensure vertical scrollability."
                    ),
                ));
            }
        });
        cx.draw_frame()
            .expect("draw chat frame with autoscroll pinned");
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            let offset_y = pane.chat.scroll.offset().y;
            let max_y = pane.chat.scroll.max_offset().y;
            assert!(max_y > px(0.0), "content should be scrollable");
            assert_eq!(
                offset_y, -max_y,
                "autoscroll must be clamped to the bottom offset"
            );
        });

        // Append more content while autoscroll is true
        cx.update_window_entity(&state, |s, _, cx| {
            let pane = s.pane_mut(id).unwrap();
            for i in 15..25 {
                pane.chat.messages.push(ChatMessage::new(
                    ChatRole::Assistant,
                    format!("New streamed chunk {i} making the conversation much taller!"),
                ));
            }
            cx.notify();
        });
        cx.draw_frame()
            .expect("draw frame after new streamed chunks");
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            let offset_y = pane.chat.scroll.offset().y;
            let max_y = pane.chat.scroll.max_offset().y;
            assert_eq!(
                offset_y, -max_y,
                "autoscroll must follow growing content to the new bottom"
            );
        });
        // User scrolls up to read history -> unpins autoscroll
        cx.update_window_entity(&state, |s, _, cx| {
            let pane = s.pane_mut(id).unwrap();
            pane.chat.autoscroll = false;
            let max_y = pane.chat.scroll.max_offset().y;
            pane.chat
                .scroll
                .set_offset(point(px(0.), -max_y + px(400.0)));
            cx.notify();
        });
        cx.draw_frame()
            .expect("draw chat frame with autoscroll unpinned");
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            assert!(!pane.chat.autoscroll, "autoscroll should be unpinned");
        });

        // Submitting a new prompt must re-pin autoscroll
        cx.update_window_entity(&state, |s, window, cx| {
            let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
            input.update(cx, |input, cx| input.set_value("new prompt", window, cx));
            s.execute_pane_input(id, window, cx);
        });
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            assert!(
                pane.chat.autoscroll,
                "sending a new prompt must re-pin autoscroll to the bottom"
            );
        });
        cx.draw_frame().unwrap();
    }

    #[test]
    fn model_selection_does_not_reauthenticate_ready_provider() {
        use gpui::Modifiers;
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
        let model = agent_auth::AgentModel {
            provider: "google-antigravity".into(),
            id: "gemini-example".into(),
            backend: agent_auth::ModelBackend::Omp,
        };
        cx.update_window_entity(&state, |s, window, cx| {
            s.set_chat_mode(id, true, window, cx);
            let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
            input.update(cx, |input, cx| input.set_value("/model", window, cx));
            s.execute_pane_input(id, window, cx);
            let auth = s.pane_mut(id).unwrap().chat.auth.as_mut().unwrap();
            auth.discovery = None;
            auth.model_discovery = None;
            auth.providers = vec![agent_auth::Provider {
                id: "google-antigravity".into(),
                ready: true,
                source: Some("omp OAuth".into()),
            }];
            auth.models = vec![model.clone()];
        });
        cx.draw_frame().unwrap();
        state.read_with(&cx, |s, _| {
            let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
            assert!(matches!(auth.page, AgentDialogPage::Models));
            assert!(auth.login.is_none());
        });
        assert!(
            cx.debug_bounds("auth-provider-0").is_none(),
            "model page must not show login rows"
        );
        let bounds = cx.debug_bounds("agent-auth-tab").unwrap();
        let center = point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_mouse_move(center, None, Modifiers::default());
        cx.simulate_click(center, Modifiers::default());
        cx.draw_frame().unwrap();
        let bounds = cx.debug_bounds("auth-provider-0").unwrap();
        let center = point(
            bounds.origin.x + px(10.0),
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_mouse_move(center, None, Modifiers::default());
        cx.simulate_click(center, Modifiers::default());
        cx.draw_frame().unwrap();
        state.read_with(&cx, |s, _| {
            let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
            assert!(matches!(auth.page, AgentDialogPage::Models));
            assert!(
                auth.login.is_none(),
                "clicking a ready provider must not start a second login"
            );
        });
        let bounds = cx.debug_bounds("agent-model-0").unwrap();
        let center = point(
            bounds.origin.x + px(10.0),
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_mouse_move(center, None, Modifiers::default());
        cx.simulate_click(center, Modifiers::default());
        state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            assert!(pane.chat.auth.is_none());
            assert_eq!(pane.chat.selected_model.as_ref(), Some(&model));
        });
        cx.update_window_entity(&state, |s, window, cx| {
            s.open_agent_auth(id, AgentDialogPage::Models, window, cx);
        });
        cx.draw_frame().unwrap();
        cx.simulate_keystrokes("escape");
        state.read_with(&cx, |s, _| {
            assert!(s.pane_ref(id).unwrap().chat.auth.is_none())
        });
    }

    #[test]
    fn test_api_key_input_typing() {
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);

        cx.update_window_entity(&state, |s, window, cx| {
            s.open_agent_auth(id, AgentDialogPage::Auth, window, cx);
            s.select_provider_for_auth(id, "commandcode".into(), window, cx);
        });
        cx.draw_frame().unwrap();

        let focused = state.read_with(&cx, |s, _| {
            let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
            auth.selected.clone()
        });
        assert_eq!(focused.as_deref(), Some("commandcode"));
        cx.run_until_parked();
        cx.draw_frame().unwrap();

        cx.simulate_input("my-secret-key-123");
        cx.draw_frame().unwrap();

        let (auth_val, bottom_val) = state.read_with(&cx, |s, cx| {
            let pane = s.pane_ref(id).unwrap();
            let auth = pane.chat.auth.as_ref().unwrap();
            let auth_v = auth.input.read(cx).value().to_string();
            let bottom_v = pane.input_state.as_ref().unwrap().read(cx).value().to_string();
            (auth_v, bottom_v)
        });
        assert_eq!(auth_val, "my-secret-key-123");
        assert_eq!(bottom_val, "");

        cx.update_window_entity(&state, |s, window, cx| {
            s.submit_agent_auth(id, window, cx);
        });
        cx.draw_frame().unwrap();

        let output = state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            let auth = pane.chat.auth.as_ref().unwrap();
            auth.output.clone()
        });
        assert!(output.contains("Connected to commandcode"));

        // Now test disconnecting
        cx.update_window_entity(&state, |s, window, cx| {
            s.disconnect_agent_provider(id, "commandcode".into(), window, cx);
        });
        cx.draw_frame().unwrap();

        let is_now_ready = state.read_with(&cx, |s, _| {
            let pane = s.pane_ref(id).unwrap();
            let auth = pane.chat.auth.as_ref().unwrap();
            auth.providers.iter().any(|p| p.id == "commandcode" && p.ready)
        });
        assert!(!is_now_ready, "commandcode must be disconnected");
    }

    #[test]
    fn model_tab_lists_only_connected_provider_models() {
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
        let connected = agent_auth::AgentModel {
            provider: "google-antigravity".into(),
            id: "gemini-connected".into(),
            backend: agent_auth::ModelBackend::Omp,
        };
        let disconnected = agent_auth::AgentModel {
            provider: "anthropic".into(),
            id: "claude-disconnected".into(),
            backend: agent_auth::ModelBackend::Rpi,
        };
        cx.update_window_entity(&state, |s, window, cx| {
            s.set_chat_mode(id, true, window, cx);
            let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
            input.update(cx, |input, cx| input.set_value("/model", window, cx));
            s.execute_pane_input(id, window, cx);
            let auth = s.pane_mut(id).unwrap().chat.auth.as_mut().unwrap();
            auth.discovery = None;
            auth.model_discovery = None;
            auth.providers = vec![
                agent_auth::Provider {
                    id: "anthropic".into(),
                    ready: false,
                    source: None,
                },
                agent_auth::Provider {
                    id: "google-antigravity".into(),
                    ready: true,
                    source: Some("omp OAuth".into()),
                },
            ];
            auth.models = vec![disconnected.clone(), connected.clone()];
        });
        cx.draw_frame().expect("draw model tab");
        assert!(
            cx.debug_bounds("agent-model-0").is_some(),
            "connected provider models are listed"
        );
        assert!(
            cx.debug_bounds("agent-model-1").is_none(),
            "models from unconnected providers are hidden"
        );
        state.read_with(&cx, |s, _| {
            let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
            assert_eq!(auth.models.len(), 2);
            assert_eq!(auth.providers.len(), 2);
        });
    }

    #[test]
    fn kathamo_bottom_input_history_and_execution() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Execute "echo 1"
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("echo 1", window, cx);
            });
            state.execute_pane_input(pane_id, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.command_history, vec!["echo 1"]);
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "");
        });

        // Execute "echo 2"
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("echo 2", window, cx);
            });
            state.execute_pane_input(pane_id, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.command_history, vec!["echo 1", "echo 2"]);
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "");
        });

        // Start typing a draft "draft cmd"
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("draft cmd", window, cx);
            });
            // Press Up: should recall "echo 2" and store "draft cmd" in pending_draft
            state.step_pane_history(pane_id, true, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.pending_draft, "draft cmd");
            assert_eq!(pane.history_index, Some(1));
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "echo 2");
        });

        // Press Up again: should recall "echo 1"
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.step_pane_history(pane_id, true, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.history_index, Some(0));
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "echo 1");
        });

        // Press Down: should recall "echo 2"
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.step_pane_history(pane_id, false, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.history_index, Some(1));
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "echo 2");
        });

        // Press Down again: should restore pending_draft "draft cmd"
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.step_pane_history(pane_id, false, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.history_index, None);
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "draft cmd");
        });
    }

    #[test]
    fn kathamo_bottom_input_clear_and_cancel() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Type a value and then clear it
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("to be cleared", window, cx);
            });
            state.clear_pane_input(pane_id, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.history_index, None);
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "");
        });

        // Cancel job should be callable without panicking
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.cancel_pane_job(pane_id, cx);
        });
    }

    #[test]
    fn terminal_is_read_only_and_cannot_be_written_directly() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        // Terminal must be configured read-only
        let is_ro = app_state.read_with(&cx, |state, cx| {
            state.projects[0].sessions[0].panes[0]
                .terminal
                .read(cx)
                .is_read_only()
        });
        assert!(is_ro, "Terminal must be strictly read-only");

        // Attempting to paste on the terminal directly is ignored
        cx.update_window_entity(&app_state, |state, _window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.terminal.update(cx, |term, cx| {
                term.paste_text("injected text", cx);
            });
        });
    }

    #[test]
    fn plus_menu_and_message_placeholder() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Check plus menu toggle
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.toggle_plus_menu(pane_id, cx);
        });
        let menu_open = app_state.read_with(&cx, |state, _| {
            state.projects[0].sessions[0].panes[0].plus_menu_open
        });
        assert!(menu_open);

        cx.draw_frame().expect("draw frame with plus menu open");

        // Run quick action "clear"
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.run_quick_action(pane_id, "clear", window, cx);
        });

        let menu_closed = app_state.read_with(&cx, |state, _| {
            !state.projects[0].sessions[0].panes[0].plus_menu_open
        });
        assert!(menu_closed);

        let history = app_state.read_with(&cx, |state, _| {
            state.projects[0].sessions[0].panes[0]
                .command_history
                .clone()
        });
        assert_eq!(history, vec!["clear"]);
    }

    #[test]
    fn terminal_shortcuts_work_in_read_only() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Ctrl+A selects all
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.terminal.update(cx, |term, cx| {
                let ev = gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("ctrl-a").unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                };
                term.dispatch_key(&ev, window, cx);
            });
        });

        // Ctrl+C with no selection sends SIGINT (\x03)
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.terminal.update(cx, |term, cx| {
                term.clear_selection(cx);
                let ev = gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("ctrl-c").unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                };
                term.dispatch_key(&ev, window, cx);
            });
        });

        // In bottom input: Ctrl+Z, Ctrl+D, Ctrl+L work
        cx.update_window_entity(&app_state, |state, window, cx| {
            let ev_z = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-z").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev_z, window, cx);

            let ev_l = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-l").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev_l, window, cx);
        });
    }

    #[test]
    fn git_branch_detection_and_badges() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        // Verify git branch detection on the current repo
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let branch = AppState::detect_git_branch(manifest_dir);
        assert!(branch.is_some(), "Should detect git branch for repo");

        // Temporary directory without git should return None
        let tmp = std::env::temp_dir();
        let non_git = AppState::detect_git_branch(&tmp);
        assert!(non_git.is_none() || tmp.join(".git").exists());

        // Verify placeholder is "Run commands"
        let placeholder_is_run_commands = app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.read(cx).placeholder_str() == "Run commands"
        });
        assert!(
            placeholder_is_run_commands,
            "Input placeholder must be 'Run commands'"
        );

        // Frame draws with badges and input
        cx.draw_frame().expect("draw frame with badges and input");
    }

    #[test]
    fn dynamic_auto_focus() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let mut is_focused = false;
        let mut pane_id = 0;
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            pane_id = pane.id;
            is_focused = input.read(cx).focus_handle().is_focused(window);
        });
        assert!(
            is_focused,
            "Bottom input must be dynamically focused on start"
        );

        // Focus pane should keep input focused
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.focus_pane(pane_id, window, cx);
        });
        cx.draw_frame().expect("draw frame after focus_pane");

        let mut input_focused_again = false;
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input_focused_again = input.read(cx).focus_handle().is_focused(window);
        });
        assert!(input_focused_again, "Input must remain focused dynamically");
    }

    #[test]
    fn terminal_right_click_copy_and_paste() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Case 1: Select text in terminal -> right-click opens context menu with Copy option
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.terminal.update(cx, |term, cx| {
                term.select_all(cx);
            });
            state.open_terminal_context_menu(pane_id, point(px(100.0), px(100.0)), window, cx);
        });

        let is_menu_open =
            app_state.read_with(&cx, |state, _| state.terminal_context_menu.is_some());
        assert!(is_menu_open, "Right click must open context menu modal");
        cx.draw_frame().expect("draw context menu modal");

        // Clicking Copy in modal copies selection and clears selection
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.copy_terminal_selection(pane_id, window, cx);
        });

        let has_sel = app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            pane.terminal.read(cx).has_selection()
        });
        assert!(!has_sel, "Copy should clear selection after copying");
        assert!(app_state.read_with(&cx, |state, _| state.terminal_context_menu.is_none()));

        // Case 2: Right-click opens modal -> clicking Paste inserts clipboard text into input
        cx.update_window_entity(&app_state, |state, window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("echo copied".to_string()));
            state.open_terminal_context_menu(pane_id, point(px(100.0), px(100.0)), window, cx);
            state.paste_from_clipboard_into_pane(pane_id, window, cx);
        });

        let input_val = app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.read(cx).value().to_string()
        });
        assert_eq!(
            input_val, "echo copied",
            "Paste should insert clipboard text into input"
        );

        // Case 3: Pasting text containing newlines should sanitize and NOT panic
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("", window, cx);
            });
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                "multi\nline\r\ncommand\n".to_string(),
            ));
            state.paste_from_clipboard_into_pane(pane_id, window, cx);
        });

        // Draw frame: if newlines leaked into single-line shape_line, this would panic
        cx.draw_frame()
            .expect("drawing frame after multiline paste must not panic");

        let input_val_clean = app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.read(cx).value().to_string()
        });
        assert_eq!(input_val_clean, "multi line command");
    }

    #[test]
    fn input_yields_to_command_and_returns_at_prompt() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");
        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
        assert!(app_state.read_with(&cx, |state, cx| {
            state
                .pane_ref(pane_id)
                .unwrap()
                .should_show_bottom_input(cx)
        }));

        cx.update_window_entity(&app_state, |state, _, cx| {
            state.on_shell_event(
                pane_id,
                &ShellEvent::CommandStarted {
                    command_line: None,
                    cwd: None,
                    cols: 80,
                    rows: 24,
                },
                cx,
            );
        });
        cx.draw_frame().expect("draw running command");
        app_state.read_with(&cx, |state, cx| {
            let pane = state.pane_ref(pane_id).unwrap();
            assert!(!pane.should_show_bottom_input(cx));
            assert!(!pane.terminal.read(cx).is_read_only());
        });

        cx.update_window_entity(&app_state, |state, _, cx| {
            state.on_shell_event(pane_id, &ShellEvent::PromptStart, cx);
        });
        cx.draw_frame().expect("draw returned prompt");
        app_state.read_with(&cx, |state, cx| {
            let pane = state.pane_ref(pane_id).unwrap();
            assert!(pane.should_show_bottom_input(cx));
            assert!(pane.terminal.read(cx).is_read_only());
        });
    }

    #[test]
    fn prompt_submission_sends_text_and_enter_without_manual_intervention() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("prompt message", window, cx);
            });
            state.execute_pane_input(pane_id, window, cx);
        });

        // Advance executor so the 25ms timer fires and delivers Enter
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(50));
        cx.run_until_parked();

        // Terminal received input and cleared draft
        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "");
            assert_eq!(
                pane.command_history.last().map(|s| s.as_str()),
                Some("prompt message")
            );
        });
    }

    #[test]
    fn image_attachments_stage_send_and_open_viewer() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let path =
            std::env::temp_dir().join(format!("kathamo-ui-attachment-{}.png", std::process::id()));
        image::RgbaImage::from_pixel(4, 3, image::Rgba([200, 80, 20, 255]))
            .save(&path)
            .expect("write test image");
        let attachment = attachments::from_path(&path).expect("test image is valid");
        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        cx.update_window_entity(&app_state, |state, window, cx| {
            assert!(state.attach_image(pane_id, attachment, window, cx));
        });
        cx.draw_frame().expect("frame with attachment strip");

        app_state.read_with(&cx, |state, _| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(pane.attachments.len(), 1);
        });

        // Submitting sends the path with the prompt and clears the strip;
        // command history keeps only what the user typed.
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |input, cx| input.set_value("what is this", window, cx));
            state.execute_pane_input(pane_id, window, cx);
        });
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(100));
        cx.run_until_parked();

        app_state.read_with(&cx, |state, _| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert!(pane.attachments.is_empty());
            assert_eq!(pane.command_history, vec!["what is this"]);
        });

        let attachment = attachments::from_path(&path).expect("test image is still valid");
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.open_image_viewer(&attachment, cx);
        });
        cx.draw_frame().expect("frame with image viewer");
        app_state.read_with(&cx, |state, _| {
            assert_eq!(
                state.image_viewer.as_ref().map(|viewer| viewer.dimensions),
                Some(Some((4, 3)))
            );
        });

        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.image_viewer = None;
            cx.notify();
        });
        cx.draw_frame().expect("frame after closing viewer");

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn copied_image_path_attaches_only_inside_an_agent() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let path = std::env::temp_dir().join(format!(
            "kathamo-copied-image-path-{}.png",
            std::process::id()
        ));
        image::RgbaImage::from_pixel(2, 2, image::Rgba([5, 90, 150, 255]))
            .save(&path)
            .expect("write test image");
        let item = gpui::ClipboardItem::new_string(path.display().to_string());
        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // A shell pane keeps a copied path as text.
        cx.update_window_entity(&app_state, |state, window, cx| {
            assert!(!state.attach_images_from_item(pane_id, &item, window, cx));
        });

        // An agent pane recognizes it as an image and stages a thumbnail.
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.projects[0].sessions[0].panes[0].is_agent = true;
            assert!(state.attach_images_from_item(pane_id, &item, window, cx));
            assert_eq!(state.projects[0].sessions[0].panes[0].attachments.len(), 1);
        });
        cx.draw_frame().expect("frame with path-derived attachment");

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn ctrl_c_cancels_draft_and_interrupts_running_process() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // 1. Typing draft and pressing Ctrl+C clears draft and sends SIGINT
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("some draft command", window, cx);
            });
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-c").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(
                input.read(cx).value().as_str(),
                "",
                "Ctrl+C must clear input draft"
            );
        });

        // 2. Pressing Ctrl+C while command is working interrupts and restores idle
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.status = SessionStatus::Working;
            state.handle_pane_ctrl_c(pane_id, window, cx);
        });

        app_state.read_with(&cx, |state, _| {
            let pane = &state.projects[0].sessions[0].panes[0];
            assert_eq!(
                pane.status,
                SessionStatus::Idle,
                "Ctrl+C must interrupt working command"
            );
            assert!(
                pane.bottom_input_visible,
                "Bottom input must be restored after Ctrl+C"
            );
        });
    }

    #[test]
    fn native_terminal_readline_shortcuts_in_input() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Test Ctrl+U (clear line)
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("command to clear", window, cx);
            });
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-u").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(
                input.read(cx).value().as_str(),
                "",
                "Ctrl+U must clear line"
            );
        });

        // Test Ctrl+W (delete previous word)
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("git checkout main", window, cx);
            });
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-w").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(
                input.read(cx).value().trim_end(),
                "git checkout",
                "Ctrl+W must delete last word"
            );
        });
    }

    #[test]
    fn autosuggestion_tab_and_right_arrow_completion() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Populate history
        cx.update_window_entity(&app_state, |state, _window, _cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.command_history
                .push("docker compose up -d".to_string());
        });

        // Type prefix "doc" and hit Tab
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("doc", window, cx);
            });
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("tab").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(
                input.read(cx).value().as_str(),
                "docker compose up -d",
                "Tab must complete autosuggestion"
            );
        });
    }

    #[test]
    fn input_auto_grow_height_increases_with_characters_and_newlines() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Initially 1 line
        let initial_rows = app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.read(cx).current_rows()
        });
        assert_eq!(initial_rows, 1, "Input should start with 1 row");

        // Typing multi-line text increases row count automatically
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("line 1\nline 2\nline 3\nline 4", window, cx);
            });
        });
        cx.draw_frame().expect("draw frame with multi-line text");

        let multiline_rows = app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.read(cx).current_rows()
        });
        assert!(
            multiline_rows >= 4,
            "Input row count must increase as characters/lines increase (auto-grow)"
        );

        // Shift+Enter inserts another line
        cx.update_window_entity(&app_state, |state, window, cx| {
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("shift-enter").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });
        cx.draw_frame().expect("draw frame after shift-enter");

        let after_shift_enter_rows = app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.read(cx).current_rows()
        });
        assert!(
            after_shift_enter_rows >= 5,
            "Shift+Enter must add newline and increase height"
        );
    }

    #[test]
    fn enter_executes_command_and_does_not_insert_newline() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        // Type command "ls -la"
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("ls -la", window, cx);
            });
            // Simulate physical Enter keydown
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("enter").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        // Verify: Enter executed the command, cleared the input field, and did NOT insert newline into input!
        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(
                input.read(cx).value().as_str(),
                "",
                "Enter must execute and clear input, not insert a newline"
            );
            assert_eq!(
                pane.command_history.last().map(|s| s.as_str()),
                Some("ls -la")
            );
        });

        // Also test dispatching Enter action directly (e.g. from GPUI action system)
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("pwd", window, cx);
            });
            input.update(cx, |inp, cx| {
                inp.enter(
                    &gpui_component::input::Enter { secondary: false },
                    window,
                    cx,
                );
            });
        });

        // In auto-grow mode, enter() should NOT have inserted \n into the text
        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert!(
                !input.read(cx).value().contains('\n'),
                "Enter action must not insert newline into auto-grow prompt"
            );
        });
    }

    #[test]
    fn typing_shows_ghost_history_suggestion_and_right_accepts_it() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.command_history
                .push("docker compose up -d".to_string());
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("docker comp", window, cx);
            });
        });
        cx.run_until_parked();

        app_state.read_with(&cx, |state, cx| {
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(
                input.read(cx).inline_completion_text(),
                Some("ose up -d"),
                "typing a history prefix must show ghost text"
            );
        });

        cx.update_window_entity(&app_state, |state, window, cx| {
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("right").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(input.read(cx).value().as_str(), "docker compose up -d");
        });
    }

    #[test]
    fn completion_menu_opens_navigates_and_accepts_with_tab() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("cat Cargo", window, cx);
            });
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("tab").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        app_state.read_with(&cx, |state, _| {
            let menu = state.projects[0].sessions[0].panes[0]
                .completion_menu
                .as_ref()
                .expect("Tab must open the completion menu");
            assert_eq!(menu.items.len(), 2, "Cargo.lock and Cargo.toml match");
            assert_eq!(menu.selected, 0);
        });

        // Move the highlight down, then accept with Tab.
        cx.update_window_entity(&app_state, |state, window, cx| {
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("down").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("tab").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "cat Cargo.toml");
            assert!(pane.completion_menu.is_none(), "accepting closes the menu");
            assert!(
                pane.command_history.is_empty(),
                "completion must not execute"
            );
        });
    }

    #[test]
    fn right_and_ctrl_c_flow_through_the_action_pipeline() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        // Ghost suggestion set through the deferred change handler, then
        // accepted with Right (bound to MoveRight in the Input context).
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.command_history
                .push("docker compose up -d".to_string());
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("docker comp", window, cx);
                inp.focus(window, cx);
            });
        });
        cx.run_until_parked();
        cx.draw_frame().expect("draw frame with ghost suggestion");
        cx.simulate_keystrokes("right");
        cx.run_until_parked();

        app_state.read_with(&cx, |state, cx| {
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(input.read(cx).value().as_str(), "docker compose up -d");
        });

        // Ctrl+C (Copy in the Input context) must still interrupt and clear.
        cx.simulate_keystrokes("ctrl-c");
        cx.run_until_parked();
        app_state.read_with(&cx, |state, cx| {
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(input.read(cx).value().as_str(), "");
        });
    }

    #[test]
    fn ctrl_r_palette_accepts_with_enter_through_dispatch() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.command_history.push("git status".to_string());
            pane.command_history.push("git push".to_string());
            state.open_history_search(pane_id, window, cx);
        });
        cx.draw_frame().expect("draw palette frame");

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        app_state.read_with(&cx, |state, cx| {
            assert!(state.history_search.is_none(), "Enter closes the palette");
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(input.read(cx).value().as_str(), "git push");
        });
    }

    #[test]
    fn history_and_readline_keys_flow_through_the_action_pipeline() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.command_history.push("echo one".to_string());
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| inp.focus(window, cx));
        });
        cx.draw_frame().expect("draw focused frame");

        // Up is bound to `MoveUp` in the Input context; at the first line it
        // must propagate so history recall runs.
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        app_state.read_with(&cx, |state, cx| {
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(input.read(cx).value().as_str(), "echo one");
        });

        // Ctrl+A is bound to SelectAll in the Input context; in an auto-grow
        // prompt it must end up as readline "start of line".
        cx.update_window_entity(&app_state, |state, window, cx| {
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("git status", window, cx);
                inp.focus(window, cx);
            });
        });
        cx.draw_frame().expect("draw frame before Ctrl+A");
        cx.simulate_keystrokes("ctrl-a");
        cx.run_until_parked();
        app_state.read_with(&cx, |state, cx| {
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(input.read(cx).cursor(), 0);
        });
    }

    #[test]
    fn enter_key_executes_once_through_the_action_pipeline() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("echo dispatched", window, cx);
                inp.focus(window, cx);
            });
        });
        cx.draw_frame().expect("draw frame with command");

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(input.read(cx).value().as_str(), "");
            assert_eq!(
                pane.command_history.last().map(String::as_str),
                Some("echo dispatched")
            );
        });
    }

    #[test]
    fn tab_key_dispatches_completion_not_indentation() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            input.update(cx, |inp, cx| {
                inp.set_value("cat Cargo", window, cx);
                inp.focus(window, cx);
            });
        });
        cx.draw_frame().expect("draw frame with input focused");

        // Goes through the real keybinding (`IndentInline` in the Input
        // context) and then bubbles to the wrapper's completion handler.
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();

        app_state.read_with(&cx, |state, cx| {
            let pane = &state.projects[0].sessions[0].panes[0];
            let input = pane.input_state.as_ref().unwrap();
            assert_eq!(
                input.read(cx).value().as_str(),
                "cat Cargo",
                "Tab must not insert indentation into the prompt"
            );
            assert!(
                pane.completion_menu.is_some(),
                "Tab must open the completion menu through the action pipeline"
            );
        });
    }

    #[test]
    fn ctrl_r_history_search_filters_and_inserts() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.draw_frame().expect("draw initial frame");

        let pane_id =
            app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

        cx.update_window_entity(&app_state, |state, window, cx| {
            let pane = &mut state.projects[0].sessions[0].panes[0];
            pane.command_history.push("cargo test".to_string());
            pane.command_history.push("git status".to_string());
            pane.command_history.push("git push".to_string());
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-r").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            state.handle_pane_input_key(pane_id, &ev, window, cx);
            assert!(
                state.history_search.is_some(),
                "Ctrl+R opens history search"
            );
        });

        // Type a query into the palette; the deferred handler refreshes it.
        cx.update_window_entity(&app_state, |state, window, cx| {
            let search = state.history_search.as_ref().unwrap();
            let input = search.input.clone();
            input.update(cx, |inp, cx| {
                inp.set_value("git", window, cx);
            });
        });
        cx.run_until_parked();

        app_state.read_with(&cx, |state, _| {
            let search = state.history_search.as_ref().unwrap();
            assert_eq!(search.matches, vec!["git push", "git status"]);
        });

        cx.update_window_entity(&app_state, |state, window, cx| {
            state.accept_history_search(window, cx);
        });

        app_state.read_with(&cx, |state, cx| {
            assert!(
                state.history_search.is_none(),
                "accepting closes the palette"
            );
            let input = state.projects[0].sessions[0].panes[0]
                .input_state
                .as_ref()
                .unwrap();
            assert_eq!(input.read(cx).value().as_str(), "git push");
        });
    }
}
