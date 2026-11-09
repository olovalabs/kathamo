use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ChatMessage, ChatRole, is_agent_tool};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub title: String,
    pub status: String,
    pub transcript: Option<PathBuf>,
    pub result: Option<String>,
}

impl Worker {
    pub fn display_title(&self) -> String {
        let mut title = String::new();
        let mut chars = self.title.chars().peekable();
        let mut previous: Option<char> = None;
        while let Some(character) = chars.next() {
            if matches!(character, '_' | '-') {
                if !title.ends_with(' ') {
                    title.push(' ');
                }
            } else {
                if character.is_uppercase()
                    && previous.is_some_and(|prev| {
                        prev.is_lowercase()
                            || (prev.is_uppercase()
                                && chars.peek().is_some_and(|next| next.is_lowercase()))
                    })
                {
                    title.push(' ');
                }
                title.push(character);
            }
            previous = Some(character);
        }
        crate::title_case(title.trim())
    }

    pub fn status_label(&self) -> &'static str {
        match self.status.as_str() {
            "pending" | "queued" => "Starting",
            "running" => "Running",
            "completed" | "success" | "succeeded" | "done" => "Completed",
            "failed" | "error" => "Failed",
            "cancelled" | "canceled" | "aborted" => "Cancelled",
            "idle" => "Idle",
            _ => "Reported",
        }
    }

    fn update(&mut self, next: &Self) {
        self.title.clone_from(&next.title);
        self.status.clone_from(&next.status);
        if next.transcript.is_some() {
            self.transcript.clone_from(&next.transcript);
        }
        if next.result.is_some() {
            self.result.clone_from(&next.result);
        }
    }
}

pub fn update_workers(current: &mut Vec<Worker>, updates: Vec<Worker>) {
    for worker in updates {
        if let Some(previous) = current.iter_mut().find(|previous| previous.id == worker.id) {
            previous.update(&worker);
        } else {
            current.push(worker);
        }
    }
}

fn transcript_path(session: &Path, id: &str) -> Option<PathBuf> {
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\', ':', '\0']) {
        return None;
    }
    Some(session.with_extension("").join(format!("{id}.jsonl")))
}

fn result_body(text: &str) -> &str {
    if text.starts_with("<task-result ") {
        let result = text
            .split_once("</task-result>")
            .map_or(text, |(body, _)| body);
        if let Some((_, output)) = result.split_once("<output>") {
            return output.strip_suffix("</output>\n").unwrap_or(output).trim();
        }
        return result
            .split_once('>')
            .map_or(result, |(_, body)| body)
            .trim();
    }
    text.trim()
}

pub fn structured(name: &str, details: &Value, session: Option<&Path>) -> Vec<Worker> {
    let jobs = details["jobs"].as_array();
    let entries = if is_agent_tool(name) {
        details["progress"]
            .as_array()
            .or_else(|| details["results"].as_array())
    } else if name.eq_ignore_ascii_case("wait")
        && jobs.is_some_and(|jobs| jobs.iter().all(|job| job["type"] == "task"))
    {
        jobs
    } else {
        None
    };
    entries
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry["agentUrlId"]
                .as_str()
                .or_else(|| entry["id"].as_str())?;
            let title = entry["label"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            Some(Worker {
                id: id.to_owned(),
                title: title.to_owned(),
                status: entry["status"]
                    .as_str()
                    .unwrap_or_else(|| {
                        if entry["aborted"] == true {
                            "cancelled"
                        } else if let Some(exit) = entry["exitCode"].as_i64() {
                            if exit == 0 && entry["error"].is_null() {
                                "completed"
                            } else {
                                "failed"
                            }
                        } else {
                            "running"
                        }
                    })
                    .to_owned(),
                transcript: session.and_then(|path| transcript_path(path, id)),
                result: entry["resultText"]
                    .as_str()
                    .or_else(|| entry["output"].as_str())
                    .map(result_body)
                    .map(str::to_owned),
            })
        })
        .collect()
}

// Older Kathamo logs only stored the rendered OMP tool output.
pub fn legacy(name: &str, text: &str) -> Vec<Worker> {
    let spawn = is_agent_tool(name) && text.starts_with("Spawned ");
    let wait = name.eq_ignore_ascii_case("wait")
        && (text.starts_with("## Completed (") || text.starts_with("## Still Running ("));
    if !spawn && !wait {
        return Vec::new();
    }
    let mut workers = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let entry = if spawn {
            line.strip_prefix("- `")
                .and_then(|line| line.split_once("` (job `"))
                .or_else(|| {
                    line.strip_prefix("Spawned agent `")
                        .and_then(|line| line.split_once('`'))
                })
                .map(|(id, _)| (id, "running"))
        } else if let Some((id, status)) = line
            .strip_prefix("### ")
            .and_then(|line| line.split_once(" [task] — "))
        {
            Some((id, status))
        } else {
            line.strip_prefix("- `")
                .and_then(|line| line.split_once("` [task] — "))
                .map(|(id, _)| (id, "running"))
        };
        if let Some((id, status)) = entry {
            workers.push(Worker {
                id: id.to_owned(),
                title: id.to_owned(),
                status: status.to_owned(),
                transcript: None,
                result: None,
            });
        } else if wait && line.starts_with("<task-result ") {
            let mut body = String::from(line);
            for line in lines.by_ref() {
                body.push('\n');
                body.push_str(line);
                if line == "</task-result>" {
                    break;
                }
            }
            if let Some(worker) = workers.last_mut() {
                worker.result = Some(result_body(&body).to_owned());
            }
        } else if wait
            && (line.starts_with("### ") || (line.starts_with("- `") && line.contains("` [")))
        {
            // A wait can report shell jobs too. Keep mixed reports visible.
            return Vec::new();
        }
    }
    workers
}

/// Legacy logs lack the OMP session path. A matching parent tool-call ID
/// identifies the session even when several chats used the same worker name.
pub fn find_legacy_transcript(id: &str, tool_id: &str) -> Option<PathBuf> {
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| Some(directories::BaseDirs::new()?.home_dir().join(".omp/agent")))?;
    for project in fs::read_dir(agent_dir.join("sessions")).ok()?.flatten() {
        let Ok(sessions) = fs::read_dir(project.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            let path = session.path();
            if path.extension().is_some() || !path.is_dir() {
                continue;
            }
            let parent = path.with_extension("jsonl");
            let child = transcript_path(&parent, id)?;
            if child.is_file() && parent_has_call(&parent, tool_id) {
                return Some(child);
            }
        }
    }
    None
}

fn parent_has_call(path: &Path, tool_id: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let reader = BufReader::new(file.take(16 * 1024 * 1024));
    reader.lines().map_while(Result::ok).any(|line| {
        if !line.contains(tool_id) {
            return false;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            return false;
        };
        let message = &record["message"];
        message["toolCallId"].as_str() == Some(tool_id)
            || message["content"].as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block["type"] == "toolCall" && block["id"].as_str() == Some(tool_id)
                })
            })
    })
}

pub struct TranscriptMessage {
    pub timestamp: Option<u64>,
    pub message: ChatMessage,
}

fn message_text(message: &Value) -> Option<String> {
    if let Some(text) = message["content"].as_str() {
        return Some(text.to_owned());
    }
    Some(
        message["content"]
            .as_array()?
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

#[derive(Default)]
pub struct LiveWorker {
    pub tool_id: Option<String>,
    pub transcript: Option<PathBuf>,
    replies: Vec<TranscriptMessage>,
}

impl LiveWorker {
    pub fn apply(&mut self, event: &Value) -> bool {
        let message = &event["message"];
        let Some(timestamp) = message["timestamp"].as_u64() else {
            return false;
        };
        let (text, settled) = match event["type"].as_str() {
            Some("message_update") if event["assistantMessageEvent"]["type"] == "text_delta" => (
                event["assistantMessageEvent"]["delta"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
                false,
            ),
            Some("message_end") if message["role"] == "assistant" => {
                (message_text(message).unwrap_or_default(), true)
            }
            _ => return false,
        };
        if text.is_empty() {
            return false;
        }
        let reply = if let Some(reply) = self
            .replies
            .last_mut()
            .filter(|reply| reply.timestamp == Some(timestamp))
        {
            reply
        } else {
            if self.replies.len() == 8 {
                self.replies.remove(0);
            }
            self.replies.push(TranscriptMessage {
                timestamp: Some(timestamp),
                message: ChatMessage::new(ChatRole::Assistant, String::new()),
            });
            self.replies.last_mut().expect("reply was just pushed")
        };
        if settled {
            reply.message.text = text;
            reply.message.streaming = false;
            reply.message.stream = None;
        } else {
            let scan = !reply.message.streaming || text.contains('\n');
            reply.message.text.push_str(&text);
            reply.message.streaming = true;
            crate::refresh_stream_view(&mut reply.message, scan);
        }
        true
    }

    pub fn unsaved_replies<'a>(
        &'a self,
        saved: &'a [TranscriptMessage],
    ) -> impl Iterator<Item = &'a ChatMessage> {
        self.replies
            .iter()
            .filter(move |reply| {
                !saved.iter().any(|entry| {
                    entry.message.role == ChatRole::Assistant && entry.timestamp == reply.timestamp
                })
            })
            .map(|reply| &reply.message)
    }
}

pub fn read_transcript(path: &Path) -> Option<Vec<TranscriptMessage>> {
    let directory = path.parent()?.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    if path.parent()? != directory || !path.is_file() {
        return None;
    }
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file.take(16 * 1024 * 1024));
    let messages = reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let record: Value = serde_json::from_str(&line).ok()?;
            let message = &record["message"];
            let role = match message["role"].as_str()? {
                "user" => ChatRole::User,
                "assistant" => ChatRole::Assistant,
                _ => return None,
            };
            let text = message_text(message)?;
            (!text.trim().is_empty()).then(|| TranscriptMessage {
                timestamp: message["timestamp"].as_u64(),
                message: ChatMessage::new(role, text),
            })
        })
        .collect::<Vec<_>>();
    (!messages.is_empty()).then_some(messages)
}

// The first four silhouettes follow the Codex reference. Each bit is one
// square in a five-pixel row, from most to least significant.
pub const AVATARS: [([u8; 5], u32); 12] = [
    ([0b11011, 0b00100, 0b00000, 0b00000, 0b10101], 0xf6c853ff),
    ([0b10001, 0b01010, 0b00100, 0b00100, 0b01110], 0xf6c853ff),
    ([0b01010, 0b01010, 0b10001, 0b01010, 0b10101], 0x3b97edff),
    ([0b00000, 0b01110, 0b10001, 0b01010, 0b11011], 0x3b97edff),
    ([0b00100, 0b01110, 0b11011, 0b01010, 0b00100], 0x74c996ff),
    ([0b10001, 0b11111, 0b10101, 0b01110, 0b01010], 0xcd9af3ff),
    ([0b00100, 0b01010, 0b10001, 0b01110, 0b00100], 0xf08d82ff),
    ([0b10101, 0b01110, 0b11011, 0b10001, 0b01010], 0x69c8cfff),
    ([0b10001, 0b01010, 0b01110, 0b10001, 0b11111], 0x74c996ff),
    ([0b01110, 0b10001, 0b10101, 0b10001, 0b01110], 0xcd9af3ff),
    ([0b00100, 0b01110, 0b11111, 0b10101, 0b10001], 0xf08d82ff),
    ([0b01010, 0b10101, 0b01110, 0b01010, 0b10001], 0x69c8cfff),
];

pub struct Row<'a> {
    pub index: usize,
    pub avatar: usize,
    pub worker: &'a Worker,
    pub result: Option<&'a str>,
}

pub fn rows(messages: &[ChatMessage]) -> Vec<Row<'_>> {
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut indices = std::collections::HashMap::new();
    let mut turn = 0;
    for (index, message) in messages.iter().enumerate() {
        if message.role == ChatRole::User {
            turn = index;
        }
        let Some(tool) = &message.tool else { continue };
        for worker in &tool.workers {
            // Native transcripts identify a worker across turns. Old logs only
            // have names, which may be reused by a different worker later.
            let scope = worker.transcript.as_ref().map_or(Ok(turn), Err);
            let key = (scope, worker.id.as_str());
            let slot = *indices.entry(key).or_insert(rows.len());
            if let Some(row) = rows.get_mut(slot) {
                row.worker = worker;
                row.result = worker.result.as_deref().or(row.result);
            } else {
                rows.push(Row {
                    index,
                    avatar: slot % AVATARS.len(),
                    worker,
                    result: worker.result.as_deref(),
                });
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent_bridge::AgentEvent, push_chat_event};
    use serde_json::json;

    #[test]
    fn structured_updates_keep_workers_in_their_original_rows() {
        let session = Path::new("/sessions/parent.jsonl");
        let spawn = structured(
            "task",
            &json!({"progress": [
                {"id":"AnalyzeTechStack","status":"running"},
                {"id":"AnalyzeRoutesAndUI","status":"pending"}
            ]}),
            Some(session),
        );
        let complete = structured(
            "wait",
            &json!({"jobs": [
                {"id":"job-a","agentUrlId":"AnalyzeTechStack","type":"task","status":"completed",
                 "resultText":"<task-result id=\"AnalyzeTechStack\">\n<output>\n### Report\n</output>\n</task-result>"},
            ]}),
            Some(session),
        );
        let mut messages = Vec::new();
        for (id, name, workers) in [("a", "task", spawn), ("b", "wait", complete)] {
            push_chat_event(
                &mut messages,
                AgentEvent::ToolEnd {
                    id: Some(id.into()),
                    name: name.into(),
                    output: String::new(),
                    failed: false,
                },
            );
            push_chat_event(
                &mut messages,
                AgentEvent::Subagents {
                    tool_id: id.into(),
                    workers,
                },
            );
        }
        let rows = rows(&messages);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 0);
        assert_eq!(rows[0].worker.status_label(), "Completed");
        assert_eq!(rows[0].result, Some("### Report"));
        assert_eq!(rows[1].worker.display_title(), "Analyze Routes And UI");
        assert_eq!(
            rows[1].worker.transcript.as_deref(),
            Some(Path::new("/sessions/parent/AnalyzeRoutesAndUI.jsonl"))
        );
    }

    #[test]
    fn worker_reply_streams_before_the_transcript_is_written() {
        let mut worker = LiveWorker::default();
        for (delta, expected) in [("First", "First"), (" words", "First words")] {
            assert!(worker.apply(&json!({
                "type":"message_update",
                "message":{"role":"assistant","timestamp":42},
                "assistantMessageEvent":{"type":"text_delta","delta":delta}
            })));
            let reply = worker.unsaved_replies(&[]).next().unwrap();
            assert_eq!(reply.text, expected);
            assert!(reply.streaming);
        }
        assert!(worker.apply(&json!({
            "type":"message_end",
            "message":{"role":"assistant","timestamp":42,"content":[{"type":"text","text":"First words."}]}
        })));
        let reply = worker.unsaved_replies(&[]).next().unwrap();
        assert_eq!(reply.text, "First words.");
        assert!(!reply.streaming);
        let saved = TranscriptMessage {
            timestamp: Some(42),
            message: reply.clone(),
        };
        assert_eq!(worker.unsaved_replies(&[saved]).count(), 0);
    }

    #[test]
    fn child_transcript_reads_only_visible_conversation_messages() {
        let dir = std::env::temp_dir().join(crate::paths::uid("worker-transcript"));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("worker.jsonl");
        fs::write(&path, concat!(
            "{\"type\":\"session\",\"id\":\"child\"}\n",
            "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Inspect files\"}]}}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"private\"},{\"type\":\"text\",\"text\":\"### Report\"}]}}\n",
            "{\"message\":{\"role\":\"toolResult\",\"content\":[{\"type\":\"text\",\"text\":\"tool log\"}]}}\n",
            "{\"incomplete\""
        )).unwrap();
        let messages = read_transcript(&path).unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message.text, "Inspect files");
        assert_eq!(messages[1].message.text, "### Report");
    }
}
