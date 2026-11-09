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
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 64 * 1024;

/// A conversation write, applied in send order by one writer thread.
///
/// Keeping a single global queue gives every conversation a total order: an
/// append is visible to the next append for the same file, `flush` is a real
/// barrier, and `clear_all` cannot be overtaken by appends that were queued
/// before it.
enum WriteJob {
    Append {
        path: PathBuf,
        max_bytes: u64,
        /// Owned so the writer thread, not the UI thread, applies the byte cap
        /// and JSON serialization.
        entry: Entry,
    },
    Flush {
        ack: mpsc::Sender<()>,
    },
    Clear {
        ack: mpsc::Sender<io::Result<()>>,
    },
    /// Release one file handle when its `Log` is dropped. The file is reopened
    /// lazily if another `Log` for the same conversation appends again.
    Close {
        path: PathBuf,
    },
}

static WRITER: OnceLock<mpsc::Sender<WriteJob>> = OnceLock::new();

fn writer() -> &'static mpsc::Sender<WriteJob> {
    WRITER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<WriteJob>();
        let _ = std::thread::Builder::new()
            .name("kathamo-conversation-writer".into())
            .spawn(move || writer_loop(receiver));
        sender
    })
}

struct OpenFile {
    file: Option<File>,
    written: u64,
}

fn writer_loop(receiver: mpsc::Receiver<WriteJob>) {
    let mut files: HashMap<PathBuf, OpenFile> = HashMap::new();
    while let Ok(job) = receiver.recv() {
        match job {
            WriteJob::Append {
                path,
                max_bytes,
                entry,
            } => {
                let write_start = std::time::Instant::now();
                let mut line = match serde_json::to_vec(&bounded_entry(entry)) {
                    Ok(line) => line,
                    Err(error) => {
                        eprintln!("failed to encode conversation {}: {error}", path.display());
                        continue;
                    }
                };
                line.push(b'\n');
                let result = write_line(&mut files, &path, max_bytes, &line);
                crate::stream_timing::record_since("transcript.write", write_start);
                if let Err(error) = result {
                    eprintln!("failed to append conversation {}: {error}", path.display());
                }
            }
            WriteJob::Flush { ack } => {
                for open in files.values_mut() {
                    if let Some(file) = open.file.as_mut() {
                        let _ = file.flush();
                    }
                }
                let _ = ack.send(());
            }
            WriteJob::Clear { ack } => {
                // Drop open handles first so the next append recreates the
                // directory instead of writing through a stale descriptor.
                files.clear();
                let result = (|| {
                    let dir = paths::conversations_dir();
                    if dir.exists() {
                        fs::remove_dir_all(&dir)?;
                    }
                    fs::create_dir_all(dir)
                })();
                let _ = ack.send(result);
            }
            WriteJob::Close { path } => {
                files.remove(&path);
            }
        }
    }
}

fn write_line(
    files: &mut HashMap<PathBuf, OpenFile>,
    path: &Path,
    max_bytes: u64,
    line: &[u8],
) -> io::Result<()> {
    let open = files.entry(path.to_path_buf()).or_insert_with(|| OpenFile {
        file: None,
        written: fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
    });
    if open.written > 0 && open.written + line.len() as u64 > max_bytes {
        open.file = None;
        let rotated = path.with_extension("jsonl.1");
        match paths::atomic_replace(path, &rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        open.written = 0;
    }
    if open.file.is_none() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        open.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
    }
    if let Some(file) = open.file.as_mut() {
        file.write_all(line)?;
    }
    open.written += line.len() as u64;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Entry {
    User { text: String },
    Event { event: AgentEvent },
    Local { role: ChatRole, text: String },
}

/// One append-only conversation file.
///
/// Appending enqueues an owned entry; the writer thread owns serialization,
/// directory creation, rotation and the file handle. The format and rotation
/// policy are unchanged, so replay reads exactly what earlier versions wrote.
pub struct Log {
    path: PathBuf,
    max_bytes: u64,
    sender: mpsc::Sender<WriteJob>,
}

impl Log {
    pub fn open(uid: &str, key: &str) -> Self {
        let path = paths::conversations_dir()
            .join(uid)
            .join(format!("{key}.jsonl"));
        Self::at_path(path, MAX_FILE_BYTES)
    }

    pub fn at_path(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            sender: writer().clone(),
        }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_user(&mut self, text: &str) {
        self.append(Entry::User {
            text: text.to_string(),
        });
    }

    pub fn append_local(&mut self, role: ChatRole, text: &str) {
        self.append(Entry::Local {
            role,
            text: text.to_string(),
        });
    }

    pub fn append_event(&mut self, event: &AgentEvent) {
        self.append(Entry::Event {
            event: event.clone(),
        });
    }

    pub fn append(&mut self, entry: Entry) {
        let enqueue_start = std::time::Instant::now();
        if self
            .sender
            .send(WriteJob::Append {
                path: self.path.clone(),
                max_bytes: self.max_bytes,
                entry,
            })
            .is_err()
        {
            eprintln!(
                "conversation writer stopped; dropped an entry for {}",
                self.path.display()
            );
        }
        crate::stream_timing::record_since("transcript.enqueue", enqueue_start);
    }

    /// Block until every write queued before this call has reached the file.
    /// Used by shutdown and by tests that immediately replay the transcript.
    pub fn flush(&mut self) {
        let (ack, receiver) = mpsc::channel();
        if self.sender.send(WriteJob::Flush { ack }).is_ok() {
            let _ = receiver.recv();
        }
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        // Order a flush and handle release behind this log's appends without
        // blocking the caller. Shutdown uses `flush` explicitly when it must
        // wait for the write to reach the file.
        let (ack, _receiver) = mpsc::channel();
        let _ = self.sender.send(WriteJob::Flush { ack });
        let _ = self.sender.send(WriteJob::Close {
            path: self.path.clone(),
        });
    }
}

fn cap_owned(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = text[..end].to_string();
    capped.push_str("… (truncated)");
    capped
}

fn bounded_entry(entry: Entry) -> Entry {
    match entry {
        Entry::User { text } => Entry::User {
            text: cap_owned(text, MAX_ENTRY_BYTES),
        },
        Entry::Local { role, text } => Entry::Local {
            role,
            text: cap_owned(text, MAX_ENTRY_BYTES),
        },
        Entry::Event { event } => Entry::Event {
            event: bounded_event(event),
        },
    }
}

fn bounded_event(event: AgentEvent) -> AgentEvent {
    match event {
        AgentEvent::Text(text) => AgentEvent::Text(cap_owned(text, MAX_ENTRY_BYTES)),
        AgentEvent::Thinking(text) => AgentEvent::Thinking(cap_owned(text, MAX_ENTRY_BYTES)),
        AgentEvent::Status(text) => AgentEvent::Status(cap_owned(text, MAX_ENTRY_BYTES)),
        AgentEvent::Tool(text) => AgentEvent::Tool(cap_owned(text, MAX_ENTRY_BYTES)),
        AgentEvent::ToolStart { id, name, input } => AgentEvent::ToolStart {
            id,
            name,
            input: cap_owned(input, MAX_ENTRY_BYTES),
        },
        AgentEvent::ToolUpdate { id, name, output } => AgentEvent::ToolUpdate {
            id,
            name,
            output: cap_owned(output, MAX_ENTRY_BYTES),
        },
        AgentEvent::ToolEnd {
            id,
            name,
            output,
            failed,
        } => AgentEvent::ToolEnd {
            id,
            name,
            output: cap_owned(output, MAX_ENTRY_BYTES),
            failed,
        },
        AgentEvent::Subagents { tool_id, workers } => AgentEvent::Subagents {
            tool_id,
            workers: workers
                .into_iter()
                .map(|mut worker| {
                    worker.result = worker.result.map(|text| cap_owned(text, MAX_ENTRY_BYTES));
                    worker
                })
                .collect(),
        },
        AgentEvent::Error(text) => AgentEvent::Error(cap_owned(text, MAX_ENTRY_BYTES)),
        AgentEvent::Title(text) => AgentEvent::Title(cap_owned(text, 200)),
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
pub fn load(path: &Path) -> Vec<std::sync::Arc<ChatMessage>> {
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
            Entry::User { text } => {
                messages.push(std::sync::Arc::new(ChatMessage::new(ChatRole::User, text)))
            }
            Entry::Local { role, text } => {
                messages.push(std::sync::Arc::new(ChatMessage::new(role, text)))
            }
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
fn normalize_replay(messages: &mut [std::sync::Arc<ChatMessage>]) {
    for message in messages {
        let message = std::sync::Arc::make_mut(message);
        message.streaming = false;
        if let Some(tool) = message.tool.as_mut()
            && tool.state == ToolState::Running
        {
            tool.state = ToolState::Unknown;
        }
    }
}

/// Delete every conversation, ordered behind all writes already queued. New
/// appends sent after this barrier are new history and are allowed to recreate
/// the directory.
pub fn clear_all() -> io::Result<()> {
    let (ack, receiver) = mpsc::channel();
    writer()
        .send(WriteJob::Clear { ack })
        .map_err(|_| io::Error::other("conversation writer stopped"))?;
    receiver
        .recv()
        .map_err(|_| io::Error::other("conversation writer did not acknowledge clear"))?
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
        log.flush();
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

        let path = log.path().to_path_buf();
        drop(log);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn queued_writes_keep_per_conversation_order() {
        let mut first = temp_log("order-first", MAX_FILE_BYTES);
        let mut second = temp_log("order-second", MAX_FILE_BYTES);
        first.append_event(&AgentEvent::Text("one".into()));
        second.append_event(&AgentEvent::Text("other".into()));
        first.append_event(&AgentEvent::Text(" two".into()));
        first.flush();

        let first_messages = load(first.path());
        assert_eq!(first_messages.len(), 1);
        assert_eq!(first_messages[0].text, "one two");
        let second_messages = load(second.path());
        assert_eq!(second_messages.len(), 1);
        assert_eq!(second_messages[0].text, "other");

        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        drop(first);
        drop(second);
        let _ = fs::remove_file(first_path);
        let _ = fs::remove_file(second_path);
    }

    #[test]
    fn running_tools_are_not_left_spinning() {
        let mut log = temp_log("running", MAX_FILE_BYTES);
        log.append_event(&AgentEvent::ToolStart {
            id: Some("t1".into()),
            name: "read".into(),
            input: "".into(),
        });
        log.flush();
        let messages = load(log.path());
        assert_eq!(messages[0].tool.as_ref().unwrap().state, ToolState::Unknown);
        let path = log.path().to_path_buf();
        drop(log);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn large_logs_rotate_and_keep_complete_lines() {
        let mut log = temp_log("rotate", 300);
        for index in 0..20 {
            log.append_event(&AgentEvent::Text(format!("line {index}")));
        }
        log.flush();
        let rotated = log.path().with_extension("jsonl.1");
        assert!(rotated.exists(), "expected the first generation to rotate");
        let current = load(log.path());
        assert!(!current.is_empty());
        assert!(fs::metadata(log.path()).unwrap().len() <= 300);
        let path = log.path().to_path_buf();
        drop(log);
        let _ = fs::remove_file(path);
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
        let capped = cap_owned(text, 10);
        assert!(capped.starts_with("ααααα"));
        assert!(capped.ends_with("(truncated)"));
    }
}
