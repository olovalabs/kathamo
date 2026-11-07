//! Append-only agent conversation history.
//!
//! Each pane (and each selected model within it) owns a JSONL file under
//! `<data>/conversations/<pane-uid>/`. Appending raw events rather than a
//! rendered transcript keeps writes cheap during streaming, and replaying the
//! events through the same reducer (`push_chat_event`) rebuilds exactly what
//! the UI showed before the restart.

use crate::agent_auth::{AgentModel, ModelBackend};
use crate::agent_bridge::AgentEvent;
use crate::paths;
use crate::{ChatMessage, ChatRole, ToolState, push_chat_event};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Entry {
    User { text: String },
    Event { event: AgentEvent },
    Local { role: ChatRole, text: String },
}

/// One append-only conversation file.
pub struct Log {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    max_bytes: u64,
}

impl Log {
    pub fn open(uid: &str, key: &str) -> Self {
        let path = paths::conversations_dir()
            .join(uid)
            .join(format!("{key}.jsonl"));
        Self::at_path(path, MAX_FILE_BYTES)
    }

    pub fn at_path(path: PathBuf, max_bytes: u64) -> Self {
        let written = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        Self {
            path,
            file: None,
            written,
            max_bytes,
        }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_user(&mut self, text: &str) {
        self.append(&Entry::User {
            text: cap_text(text, MAX_ENTRY_BYTES),
        });
    }

    pub fn append_local(&mut self, role: ChatRole, text: &str) {
        self.append(&Entry::Local {
            role,
            text: cap_text(text, MAX_ENTRY_BYTES),
        });
    }

    pub fn append_event(&mut self, event: &AgentEvent) {
        self.append(&Entry::Event {
            event: bounded_event(event),
        });
    }

    pub fn append(&mut self, entry: &Entry) {
        if let Err(error) = self.try_append(entry) {
            eprintln!(
                "failed to append conversation {}: {error}",
                self.path.display()
            );
        }
    }

    fn try_append(&mut self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        if self.written > 0 && self.written + line.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(&line)?;
            file.flush()?;
        }
        self.written += line.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let rotated = self.path.with_extension("jsonl.1");
        match paths::atomic_replace(&self.path, &rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.written = 0;
        Ok(())
    }

    pub fn flush(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        self.flush();
    }
}

fn cap_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = text[..end].to_string();
    capped.push_str("… (truncated)");
    capped
}

fn bounded_event(event: &AgentEvent) -> AgentEvent {
    match event.clone() {
        AgentEvent::Text(text) => AgentEvent::Text(cap_text(&text, MAX_ENTRY_BYTES)),
        AgentEvent::Thinking(text) => AgentEvent::Thinking(cap_text(&text, MAX_ENTRY_BYTES)),
        AgentEvent::Status(text) => AgentEvent::Status(cap_text(&text, MAX_ENTRY_BYTES)),
        AgentEvent::Tool(text) => AgentEvent::Tool(cap_text(&text, MAX_ENTRY_BYTES)),
        AgentEvent::ToolStart { id, name, input } => AgentEvent::ToolStart {
            id,
            name,
            input: cap_text(&input, MAX_ENTRY_BYTES),
        },
        AgentEvent::ToolUpdate { id, name, output } => AgentEvent::ToolUpdate {
            id,
            name,
            output: cap_text(&output, MAX_ENTRY_BYTES),
        },
        AgentEvent::ToolEnd {
            id,
            name,
            output,
            failed,
        } => AgentEvent::ToolEnd {
            id,
            name,
            output: cap_text(&output, MAX_ENTRY_BYTES),
            failed,
        },
        AgentEvent::Error(text) => AgentEvent::Error(cap_text(&text, MAX_ENTRY_BYTES)),
        AgentEvent::Done => AgentEvent::Done,
        AgentEvent::Closed => AgentEvent::Closed,
    }
}

/// File name stem for a chat: `default` for the model-less conversation, or a
/// sanitized backend/provider/model key.
pub fn conversation_key(model: Option<&AgentModel>) -> String {
    match model {
        None => "default".to_string(),
        Some(model) => {
            let backend = match model.backend {
                ModelBackend::Rpi => "rpi",
                ModelBackend::Omp => "omp",
            };
            sanitize(&format!("{backend}_{}_{}", model.provider, model.id))
        }
    }
}

fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("default");
    }
    out
}

/// Read a conversation file and rebuild the visible transcript. Missing files
/// yield an empty transcript; corrupt lines are skipped.
pub fn load(path: &Path) -> Vec<ChatMessage> {
    let Some(text) = read_bounded(path) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(line) else {
            continue;
        };
        match entry {
            Entry::User { text } => messages.push(ChatMessage::new(ChatRole::User, text)),
            Entry::Local { role, text } => messages.push(ChatMessage::new(role, text)),
            Entry::Event { event } => match event {
                AgentEvent::Done | AgentEvent::Closed => {}
                event => push_chat_event(&mut messages, event),
            },
        }
    }
    normalize_replay(&mut messages);
    messages
}

fn read_bounded(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > MAX_FILE_BYTES {
        file.seek(SeekFrom::Start(len - MAX_FILE_BYTES)).ok()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Drop the partial first line after seeking into the middle of a file.
        let start = text.find('\n').map(|index| index + 1).unwrap_or(text.len());
        Some(text[start..].to_string())
    } else {
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        Some(text)
    }
}

/// State that was mid-flight when the app quit must not look live after a
/// restart: no streaming message and no spinning tool calls.
fn normalize_replay(messages: &mut [ChatMessage]) {
    for message in messages {
        message.streaming = false;
        if let Some(tool) = message.tool.as_mut()
            && tool.state == ToolState::Running
        {
            tool.state = ToolState::Unknown;
        }
    }
}

pub fn clear_all() -> io::Result<()> {
    let dir = paths::conversations_dir();
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(name: &str, max_bytes: u64) -> Log {
        let path = std::env::temp_dir().join(format!(
            "kathamo-conversations-{name}-{}.jsonl",
            paths::uid("t")
        ));
        Log::at_path(path, max_bytes)
    }

    #[test]
    fn entries_roundtrip_as_json() {
        let entries = vec![
            Entry::User {
                text: "hello".into(),
            },
            Entry::Event {
                event: AgentEvent::Text("hi".into()),
            },
            Entry::Event {
                event: AgentEvent::ToolStart {
                    id: Some("1".into()),
                    name: "read".into(),
                    input: "{}".into(),
                },
            },
            Entry::Local {
                role: ChatRole::Status,
                text: "/fast on".into(),
            },
        ];
        for entry in entries {
            let text = serde_json::to_string(&entry).unwrap();
            let _: Entry = serde_json::from_str(&text).unwrap();
        }
    }

    #[test]
    fn transcript_replays_user_and_agent_messages() {
        let mut log = temp_log("replay", MAX_FILE_BYTES);
        log.append_user("fix the bug");
        log.append_event(&AgentEvent::Text("looking".into()));
        log.append_event(&AgentEvent::Text(" now".into()));
        log.append_event(&AgentEvent::ToolStart {
            id: Some("t1".into()),
            name: "read".into(),
            input: "src/main.rs".into(),
        });
        log.append_event(&AgentEvent::ToolEnd {
            id: Some("t1".into()),
            name: "read".into(),
            output: "ok".into(),
            failed: false,
        });
        log.append_local(ChatRole::Status, "/fast on");
        let messages = load(log.path());

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, ChatRole::User);
        assert_eq!(messages[0].text, "fix the bug");
        // Consecutive text deltas coalesce into one assistant message.
        assert_eq!(messages[1].role, ChatRole::Assistant);
        assert_eq!(messages[1].text, "looking now");
        assert!(!messages[1].streaming);
        assert_eq!(messages[2].role, ChatRole::Tool);
        let tool = messages[2].tool.as_ref().unwrap();
        assert_eq!(tool.state, ToolState::Completed);
        assert_eq!(tool.output, "ok");
        assert_eq!(messages[3].role, ChatRole::Status);
        assert_eq!(messages[3].text, "/fast on");

        let _ = fs::remove_file(log.path());
    }

    #[test]
    fn running_tools_are_not_left_spinning() {
        let mut log = temp_log("running", MAX_FILE_BYTES);
        log.append_event(&AgentEvent::ToolStart {
            id: Some("t1".into()),
            name: "read".into(),
            input: "".into(),
        });
        let messages = load(log.path());
        assert_eq!(messages[0].tool.as_ref().unwrap().state, ToolState::Unknown);
        let _ = fs::remove_file(log.path());
    }

    #[test]
    fn large_logs_rotate_and_keep_complete_lines() {
        let mut log = temp_log("rotate", 300);
        for index in 0..20 {
            log.append_event(&AgentEvent::Text(format!("line {index}")));
        }
        let rotated = log.path().with_extension("jsonl.1");
        assert!(rotated.exists(), "expected the first generation to rotate");
        let current = load(log.path());
        assert!(!current.is_empty());
        assert!(fs::metadata(log.path()).unwrap().len() <= 300);
        let _ = fs::remove_file(log.path());
        let _ = fs::remove_file(rotated);
    }

    #[test]
    fn conversation_keys_are_filesystem_safe() {
        let model = AgentModel {
            provider: "google/antigravity".into(),
            id: "gemini:3.1".into(),
            backend: ModelBackend::Omp,
        };
        let key = conversation_key(Some(&model));
        assert_eq!(key, "omp_google_antigravity_gemini_3.1");
        assert_eq!(conversation_key(None), "default");
    }

    #[test]
    fn capped_text_respects_utf8_boundaries() {
        let text = "α".repeat(100);
        let capped = cap_text(&text, 10);
        assert!(capped.starts_with("ααααα"));
        assert!(capped.ends_with("(truncated)"));
    }
}
