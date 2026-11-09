//! Opt-in baseline: the pre-embedding rpi RPC subprocess bridge.
//!
//! The embedded bridge is the default. Setting `KATHAMO_RPI_BACKEND=rpc` keeps
//! the old process/stdio/JSONL path available for before/after measurements on
//! a credentialed machine, without changing the UI or the event contract.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use serde_json::{Value, json};

use super::{AgentEvent, coalesce_events, decode_message_update, decode_tool_event};
use crate::agent_writer::AgentWriter;

enum Incoming {
    Event(AgentEvent),
    Closed(Option<String>),
}

/// Owns an rpi process and its conversation. Dropping it terminates the process.
pub(crate) struct RpcAgentBridge {
    child: Child,
    writer: AgentWriter,
    events: mpsc::Receiver<Incoming>,
    stderr: Arc<Mutex<String>>,
    closed: bool,
    next_id: u64,
    trace_id: u64,
    cwd: PathBuf,
    session_id: Arc<Mutex<Option<String>>>,
    model: Arc<Mutex<Option<String>>>,
}

impl RpcAgentBridge {
    pub(crate) fn spawn_resumed(
        cwd: &Path,
        session: Option<&str>,
        model: Option<&str>,
        trace_id: u64,
    ) -> Result<Self, String> {
        Self::spawn_session(cwd, session, model, trace_id)
    }

    pub(crate) fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|id| id.clone())
    }

    fn spawn_session(
        cwd: &Path,
        session: Option<&str>,
        model: Option<&str>,
        trace_id: u64,
    ) -> Result<Self, String> {
        Self::spawn_session_with(Path::new("rpi"), cwd, session, model, trace_id)
    }

    fn spawn_session_with(
        binary: &Path,
        cwd: &Path,
        session: Option<&str>,
        model: Option<&str>,
        trace_id: u64,
    ) -> Result<Self, String> {
        let mut command = Command::new(binary);
        command
            .args(["--mode", "rpc"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session) = session {
            command.args(["--session", session]);
        }
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let spawn_start = std::time::Instant::now();
        let mut child = command.spawn().map_err(|e| {
            format!(
                "Could not start rpi in {}: {e}. Install rpi and ensure it is on PATH.",
                cwd.display()
            )
        })?;
        crate::stream_timing::record_since("engine.rpc.spawn", spawn_start);
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(mut sink) = stderr_sink.lock() {
                            sink.push_str(&line);
                            if sink.len() > 16_384 {
                                let mut start = sink.len() - 16_384;
                                while !sink.is_char_boundary(start) {
                                    start += 1;
                                }
                                sink.drain(..start);
                            }
                        }
                    }
                }
            }
        });
        let (sender, events) = mpsc::channel();
        let error_sender = sender.clone();
        let writer = AgentWriter::spawn(stdin, move |error| {
            let _ = error_sender.send(Incoming::Event(AgentEvent::Error(error)));
            crate::agent_bridge::signal_wake();
        });
        let reply_writer = writer.clone();
        let session_id = Arc::new(Mutex::new(session.map(str::to_owned)));
        let session_sink = Arc::clone(&session_id);
        let model_id = Arc::new(Mutex::new(model.map(str::to_owned)));
        let model_sink = Arc::clone(&model_id);
        let reader_start = std::time::Instant::now();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut first_record = true;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = sender.send(Incoming::Closed(None));
                        crate::agent_bridge::signal_wake();
                        break;
                    }
                    Err(e) => {
                        let _ = sender.send(Incoming::Closed(Some(format!(
                            "Failed reading rpi output: {e}"
                        ))));
                        crate::agent_bridge::signal_wake();
                        break;
                    }
                    Ok(_) => {}
                }
                let parse_start = std::time::Instant::now();
                let record: Value = match serde_json::from_str(&line) {
                    Ok(record) => record,
                    Err(e) => {
                        let _ = sender.send(Incoming::Event(AgentEvent::Error(format!(
                            "Invalid rpi RPC output: {e}"
                        ))));
                        crate::agent_bridge::signal_wake();
                        continue;
                    }
                };
                crate::stream_timing::record_since("bridge.rpc.decode", parse_start);
                if first_record {
                    first_record = false;
                    crate::stream_timing::record_since("engine.rpc.first_record", reader_start);
                }
                if record["type"] == "response" && record["command"] == "get_state" {
                    if let Some(id) = record["data"]["sessionId"].as_str()
                        && let Ok(mut session) = session_sink.lock()
                    {
                        *session = Some(id.to_owned());
                    }
                    if let (Some(provider), Some(model)) = (
                        record["data"]["provider"].as_str(),
                        record["data"]["modelId"].as_str(),
                    ) && let Ok(mut selected) = model_sink.lock()
                    {
                        *selected = Some(format!("{provider}/{model}"));
                    }
                }
                if record["type"] == "extension_ui_request" {
                    handle_extension_ui(&record, &reply_writer, &sender);
                    crate::agent_bridge::signal_wake();
                } else {
                    let project_start = std::time::Instant::now();
                    let decoded = decode_event(&record);
                    crate::stream_timing::record_since("bridge.rpc.project", project_start);
                    for event in decoded {
                        match &event {
                            AgentEvent::Text(text) if !text.is_empty() => {
                                crate::stream_timing::turn_delta_arrived(
                                    trace_id,
                                    crate::stream_timing::DeltaKind::Answer,
                                );
                            }
                            AgentEvent::Thinking(text) if !text.is_empty() => {
                                crate::stream_timing::turn_delta_arrived(
                                    trace_id,
                                    crate::stream_timing::DeltaKind::Reasoning,
                                );
                            }
                            _ => {}
                        }
                        if sender.send(Incoming::Event(event)).is_err() {
                            return;
                        }
                    }
                    crate::agent_bridge::signal_wake();
                }
            }
        });
        writer.send_json(json!({"type": "get_state", "id": "kathamo-session"}))?;
        Ok(Self {
            child,
            writer,
            events,
            stderr,
            closed: false,
            next_id: 0,
            trace_id,
            cwd: cwd.to_path_buf(),
            session_id,
            model: model_id,
        })
    }

    pub(crate) fn restart_after_auth(&mut self) -> Result<(), String> {
        let session = self
            .session_id
            .lock()
            .map_err(|_| "rpi session lock was poisoned")?
            .clone()
            .ok_or("The agent session is still starting; try again shortly")?;
        let model = self
            .model
            .lock()
            .map_err(|_| "rpi model lock was poisoned")?
            .clone()
            .ok_or("The agent model is still starting; try again shortly")?;
        let cwd = self.cwd.clone();
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.closed = true;
        let replacement = Self::spawn_session(&cwd, Some(&session), Some(&model), self.trace_id)?;
        *self = replacement;
        Ok(())
    }

    pub(crate) fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if self.closed {
            return Err("The rpi process has exited".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = format!("kathamo-{}", self.next_id);
        self.writer.send_prompt(id, prompt.to_owned())
    }

    pub(crate) fn cancel(&mut self) -> Result<(), String> {
        self.send(json!({"type": "abort"}))
    }

    fn send(&mut self, mut command: Value) -> Result<(), String> {
        if self.closed {
            return Err("The rpi process has exited".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        command["id"] = json!(format!("kathamo-{}", self.next_id));
        self.writer.send_json(command)
    }

    pub(crate) fn poll(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(incoming) = self.events.try_recv() {
            match incoming {
                Incoming::Event(event) => events.push(event),
                Incoming::Closed(error) => {
                    if !self.closed {
                        self.closed = true;
                        self.writer.close();
                        let status = self
                            .child
                            .try_wait()
                            .ok()
                            .flatten()
                            .map(|status| format!(" ({status})"))
                            .unwrap_or_default();
                        let diagnostic = self
                            .stderr
                            .lock()
                            .ok()
                            .map(|s| s.trim().to_owned())
                            .unwrap_or_default();
                        let mut message =
                            error.unwrap_or_else(|| format!("rpi process exited{status}"));
                        if !diagnostic.is_empty() {
                            message.push_str(&format!(": {diagnostic}"));
                        }
                        events.push(AgentEvent::Error(message));
                        events.push(AgentEvent::Done);
                        events.push(AgentEvent::Closed);
                    }
                }
            }
        }
        coalesce_events(events)
    }
}

impl Drop for RpcAgentBridge {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The child is gone, so a writer blocked on a full pipe has unwound.
        self.writer.shutdown();
    }
}

fn handle_extension_ui(record: &Value, writer: &AgentWriter, sender: &mpsc::Sender<Incoming>) {
    let method = record["method"].as_str().unwrap_or("");
    match method {
        "select" | "confirm" | "input" | "editor" => {
            let title = record["title"].as_str().unwrap_or("Extension interaction");
            let _ = sender.send(Incoming::Event(AgentEvent::Error(format!("{title}: interactive extension dialogs are not supported in this view; request cancelled"))));
            if let Err(error) = writer.send_json(
                json!({"type": "extension_ui_response", "id": record["id"], "cancelled": true}),
            ) {
                let _ = sender.send(Incoming::Event(AgentEvent::Error(error)));
            }
        }
        "notify" => {
            if let Some(message) = record["message"].as_str() {
                let event = if record["notifyType"] == "error" {
                    AgentEvent::Error(message.to_owned())
                } else {
                    AgentEvent::Tool(message.to_owned())
                };
                let _ = sender.send(Incoming::Event(event));
            }
        }
        _ => {}
    }
}

fn decode_event(record: &Value) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    match record["type"].as_str().unwrap_or("") {
        "response" => {
            let command = record["command"].as_str().unwrap_or("command");
            if record["success"] == false {
                let error = record["error"].as_str().unwrap_or("unknown error");
                events.push(AgentEvent::Error(format!("rpi {command}: {error}")));
                if command == "prompt" {
                    events.push(AgentEvent::Done);
                }
            } else if command == "prompt" && record["data"]["disposition"] == "handled" {
                events.push(AgentEvent::Done);
            }
        }
        "message_update" => {
            if let Some(event) = decode_message_update(record) {
                events.push(event);
            }
        }
        "message_end" => {
            let message = &record["message"];
            if message["role"] == "assistant" && message["stopReason"] == "error" {
                let error = message["errorMessage"]
                    .as_str()
                    .unwrap_or("The agent encountered an error");
                events.push(AgentEvent::Error(error.to_owned()));
            }
        }
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            if let Some(event) = decode_tool_event(record) {
                events.push(event);
            }
        }
        "agent_settled" => events.push(AgentEvent::Done),
        "extension_error" => {
            let error = record["error"]
                .as_str()
                .unwrap_or("Unknown extension error");
            events.push(AgentEvent::Error(format!("rpi extension: {error}")));
        }
        _ => {}
    }
    events
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    #[test]
    fn a_slow_rpc_reader_does_not_block_prompt_or_abort() {
        let dir = std::env::temp_dir().join(format!("kathamo-slow-rpc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("slpw-rpi.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\nexec cat >/dev/null\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut bridge = RpcAgentBridge::spawn_session_with(&script, &dir, None, None, 1).unwrap();
        let prompt = "x".repeat(4 * 1024 * 1024);
        let start = Instant::now();
        bridge.send_prompt(&prompt).unwrap();
        let prompt_time = start.elapsed();
        let start = Instant::now();
        bridge.cancel().unwrap();
        let abort_time = start.elapsed();
        assert!(
            prompt_time < Duration::from_millis(100),
            "prompt enqueue took {prompt_time:?}"
        );
        assert!(
            abort_time < Duration::from_millis(50),
            "abort enqueue took {abort_time:?}"
        );
        drop(bridge);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
