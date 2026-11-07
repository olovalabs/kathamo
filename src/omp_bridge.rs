use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use serde_json::{Value, json};

use crate::agent_bridge::{AgentEvent, decode_message_update, decode_tool_event};

enum Incoming {
    Record(Value),
    Error(String),
    Closed,
}

/// A separate, persistent OMP conversation for one Google Antigravity model.
pub struct OmpBridge {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    events: mpsc::Receiver<Incoming>,
    next_id: u64,
    pending_prompt: Option<String>,
    closed: bool,
}

impl OmpBridge {
    pub fn spawn(cwd: &Path, model: &str) -> Result<Self, String> {
        let model = model.strip_prefix("google-antigravity/").unwrap_or(model);
        if model.is_empty() {
            return Err("No Google Antigravity model was selected".into());
        }
        let selector = format!("google-antigravity/{model}");
        let mut command = Command::new("omp");
        command
            .args(["--mode", "rpc", "--model", &selector])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // OMP diagnostics can contain provider request details. Never put them in chat.
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            format!(
                "Could not start omp in {}: {error}. Install omp and ensure it is on PATH.",
                cwd.display()
            )
        })?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, events) = mpsc::channel();
        let reply_stdin = Arc::clone(&stdin);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Err(error) => {
                        let _ = sender.send(Incoming::Error(format!(
                            "Failed reading omp output: {error}"
                        )));
                        break;
                    }
                    Ok(_) => {}
                }
                let record: Value = match serde_json::from_str(&line) {
                    Ok(record) => record,
                    Err(_) => {
                        let _ = sender.send(Incoming::Error("Invalid omp RPC output".into()));
                        continue;
                    }
                };
                if record["type"] == "extension_ui_request" {
                    match record["method"].as_str().unwrap_or("") {
                        "select" | "confirm" | "input" | "editor" | "ask" => {
                            // Never approve a request that this UI cannot show or answer.
                            let _ = sender.send(Incoming::Error(
                                "OMP requested an interactive dialog or tool approval, which is not supported in this view; the request was cancelled.".into(),
                            ));
                            if let Err(error) = write_command(
                                &reply_stdin,
                                &json!({
                                    "type": "extension_ui_response", "id": record["id"], "cancelled": true
                                }),
                            ) {
                                let _ = sender.send(Incoming::Error(error));
                            }
                        }
                        "notify" => {
                            if let Some(message) = record["message"].as_str() {
                                let event = if record["notifyType"] == "error" {
                                    Incoming::Error(message.to_owned())
                                } else {
                                    Incoming::Record(record.clone())
                                };
                                let _ = sender.send(event);
                            }
                        }
                        _ => {}
                    }
                } else if sender.send(Incoming::Record(record)).is_err() {
                    return;
                }
            }
            let _ = sender.send(Incoming::Closed);
        });
        // Request incremental updates rather than repeatedly transmitting the
        // entire growing message. Older OMP versions may reject this optional
        // projection; their ordinary full updates contain the same deltas.
        write_command(
            &stdin,
            &json!({
                "type": "set_event_filter", "id": "kathamo-event-filter",
                "events": null, "messageUpdates": "delta"
            }),
        )?;
        let _ = write_command(
            &stdin,
            &json!({
                "type": "set_cache_warming", "id": "kathamo-cache-warming", "mode": "idle"
            }),
        );
        let _ = write_command(
            &stdin,
            &json!({
                "type": "set_thinking_level", "id": "kathamo-thinking-level", "level": "low"
            }),
        );
        Ok(Self {
            child,
            stdin,
            events,
            next_id: 0,
            pending_prompt: None,
            closed: false,
        })
    }

    pub fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if self.pending_prompt.is_some() {
            return Err("An OMP prompt is already running".into());
        }
        let id = self.send(json!({"type": "prompt", "message": prompt}))?;
        self.pending_prompt = Some(id);
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        self.send(json!({"type": "abort"})).map(|_| ())
    }
    pub fn set_thinking_level(&mut self, level: &str) -> Result<(), String> {
        self.send(json!({"type": "set_thinking_level", "level": level}))
            .map(|_| ())
    }

    fn send(&mut self, mut command: Value) -> Result<String, String> {
        if self.closed {
            return Err("The omp process has exited".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = format!("kathamo-{}", self.next_id);
        command["id"] = json!(id);
        write_command(&self.stdin, &command)?;
        Ok(id)
    }

    pub fn poll(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(incoming) = self.events.try_recv() {
            match incoming {
                Incoming::Error(error) => events.push(AgentEvent::Error(error)),
                Incoming::Closed => {
                    if !self.closed {
                        self.closed = true;
                        let status = self
                            .child
                            .try_wait()
                            .ok()
                            .flatten()
                            .map(|status| format!(" ({status})"))
                            .unwrap_or_default();
                        events.push(AgentEvent::Error(format!("OMP process exited{status}. Check your OMP installation and Google Antigravity login.")));
                        if self.pending_prompt.take().is_some() {
                            events.push(AgentEvent::Done);
                        }
                        events.push(AgentEvent::Closed);
                    }
                }
                Incoming::Record(record) => self.handle_record(&record, &mut events),
            }
        }
        events
    }

    fn handle_record(&mut self, record: &Value, events: &mut Vec<AgentEvent>) {
        let matching_prompt = self
            .pending_prompt
            .as_deref()
            .is_some_and(|id| record["id"].as_str() == Some(id));
        match record["type"].as_str().unwrap_or("") {
            "response" if record["id"] == "kathamo-event-filter" => {}
            "response" if record["success"] == false => {
                let command = record["command"].as_str().unwrap_or("command");
                let detail = record["error"].as_str().unwrap_or("Unknown error");
                events.push(AgentEvent::Error(format!("OMP {command}: {detail}")));
                if matching_prompt && command == "prompt" {
                    self.pending_prompt = None;
                    events.push(AgentEvent::Done);
                }
            }
            "response"
                if matching_prompt
                    && record["command"] == "prompt"
                    && record["data"]["agentInvoked"] == false =>
            {
                // Built-in slash commands can be handled locally without a prompt_result.
                self.pending_prompt = None;
                events.push(AgentEvent::Done);
            }
            "prompt_result" if matching_prompt => {
                if record["status"] == "error" {
                    let detail = record["error"]["message"]
                        .as_str()
                        .or_else(|| record["error"].as_str())
                        .unwrap_or("Check your OMP configuration and Google Antigravity login.");
                    events.push(AgentEvent::Error(format!(
                        "OMP agent request failed: {detail}"
                    )));
                }
                self.pending_prompt = None;
                events.push(AgentEvent::Done);
            }
            "message_update" => {
                if let Some(event) = decode_message_update(record) {
                    events.push(event);
                }
            }
            "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                if let Some(event) = decode_tool_event(record) {
                    events.push(event);
                }
                let failed = record["isError"] == true;
                if record["type"] == "tool_execution_end"
                    && failed
                    && record["result"]["content"]
                        .as_array()
                        .is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block["text"].as_str().is_some_and(|text| {
                                    text.contains("requires approval")
                                        || text.contains("no interactive UI")
                                })
                            })
                        })
                {
                    events.push(AgentEvent::Error("OMP could not obtain tool approval in this view. Use an interactive OMP session or adjust your OMP approval settings.".into()));
                }
            }
            "auto_retry_start" => {
                let status = match (
                    record["attempt"].as_u64(),
                    record["maxAttempts"].as_u64(),
                    record["delayMs"].as_u64(),
                ) {
                    (Some(attempt), Some(maximum), Some(delay)) => format!(
                        "Retrying model request (attempt {attempt}/{maximum}; delay {delay} ms)."
                    ),
                    _ => "Retrying model request…".into(),
                };
                events.push(AgentEvent::Status(status));
            }
            "auto_retry_end" => {
                let status = if record["success"] == true {
                    "Retry succeeded."
                } else {
                    "Retry ended without success."
                };
                events.push(AgentEvent::Status(status.into()));
            }
            "auto_compaction_start" => {
                events.push(AgentEvent::Status("Preparing conversation context…".into()))
            }
            "auto_compaction_end" => {
                let status = if record["aborted"] == true {
                    "Conversation context preparation was cancelled."
                } else if record["skipped"] == true {
                    "Conversation context preparation was skipped."
                } else if record.get("errorMessage").and_then(Value::as_str).is_some() {
                    "Conversation context preparation failed."
                } else {
                    "Conversation context preparation finished."
                };
                events.push(AgentEvent::Status(status.into()));
            }
            "extension_ui_request" if record["method"] == "notify" => {
                if let Some(message) = record["message"].as_str() {
                    events.push(AgentEvent::Tool(message.to_owned()));
                }
            }
            "extension_error" => {
                events.push(AgentEvent::Error(
                    "An OMP extension failed during this session.".into(),
                ));
            }
            "rpc_frame_error" => {
                events.push(AgentEvent::Error(
                    "OMP could not deliver an RPC event because it exceeded the transport limit."
                        .into(),
                ));
                if record["originalType"] == "prompt_result" && self.pending_prompt.take().is_some()
                {
                    events.push(AgentEvent::Done);
                }
            }
            // agent_end is a run boundary, not necessarily completion of this prompt:
            // the correlated prompt_result follows it and reports the final outcome.
            _ => {}
        }
    }
}

impl Drop for OmpBridge {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_command(stdin: &Mutex<ChildStdin>, command: &Value) -> Result<(), String> {
    let mut input = stdin
        .lock()
        .map_err(|_| "OMP input lock was poisoned".to_string())?;
    serde_json::to_writer(&mut *input, command)
        .map_err(|error| format!("Failed to encode OMP command: {error}"))?;
    input
        .write_all(b"\n")
        .and_then(|_| input.flush())
        .map_err(|error| format!("Failed to send OMP command: {error}"))
}
