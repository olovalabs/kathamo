use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use serde_json::{Value, json};

use crate::agent_bridge::{AgentEvent, coalesce_events, decode_message_update, decode_tool_event};

enum Incoming {
    /// A decoded record. `event` is the projected assistant delta or tool
    /// lifecycle event, computed on the reader thread so the UI thread only
    /// has to apply it.
    Record {
        record: Value,
        event: Option<AgentEvent>,
        /// Precomputed on the reader thread: this failed tool result is an
        /// approval request that this view could not answer.
        approval_error: bool,
    },
    Error(String),
    /// A background read of OMP's session-file title finished.
    TitleRead(Option<String>),
    Closed,
}

/// A separate, persistent OMP conversation for one Google Antigravity model.
pub struct OmpBridge {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    events: mpsc::Receiver<Incoming>,
    sender: mpsc::Sender<Incoming>,
    next_id: u64,
    pending_prompt: Option<String>,
    closed: bool,
    /// Where OMP persists this session, learned from `get_state`. OMP writes its
    /// auto-generated conversation title into the first line of that file.
    session_file: Option<PathBuf>,
    title: Option<String>,
    /// OMP only names a session when asked, so the first completed turn
    /// triggers its title generator once.
    title_requested: bool,
    /// A title request is running as background work. A prompt that arrives
    /// meanwhile cancels it rather than failing as "already processing".
    title_in_flight: bool,
    /// The session file is being read on a background thread.
    title_read_in_flight: bool,
    live_workers: HashMap<String, crate::subagent_chat::LiveWorker>,
    worker_changed: bool,
}

impl OmpBridge {
    pub fn spawn(cwd: &Path, model: &str) -> Result<Self, String> {
        let model = model.trim();
        if model.is_empty() {
            return Err("No model was selected".into());
        }
        let selector = if model.contains('/') {
            model.to_string()
        } else {
            format!("google-antigravity/{model}")
        };
        let mut command = Command::new("omp");
        command
            .args(["--mode", "rpc", "--model", &selector])
            .args(["--append-system-prompt", crate::agent_ui::CATALOG_PROMPT])
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
        let spawn_start = std::time::Instant::now();
        let mut child = command.spawn().map_err(|error| {
            format!(
                "Could not start omp in {}: {error}. Install omp and ensure it is on PATH.",
                cwd.display()
            )
        })?;
        crate::stream_timing::record_since("engine.omp.spawn", spawn_start);
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, events) = mpsc::channel();
        let bridge_sender = sender.clone();
        let reply_stdin = Arc::clone(&stdin);
        let reader_start = std::time::Instant::now();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut first_record = true;
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
                let parse_start = std::time::Instant::now();
                let record: Value = match serde_json::from_str(&line) {
                    Ok(record) => record,
                    Err(_) => {
                        let _ = sender.send(Incoming::Error("Invalid omp RPC output".into()));
                        continue;
                    }
                };
                crate::stream_timing::record_since("bridge.omp.decode", parse_start);
                if first_record {
                    first_record = false;
                    crate::stream_timing::record_since("engine.omp.first_record", reader_start);
                }
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
                                    Incoming::Record {
                                        record: record.clone(),
                                        event: None,
                                        approval_error: false,
                                    }
                                };
                                let _ = sender.send(event);
                            }
                        }
                        _ => {}
                    }
                } else {
                    // Project the two hot record kinds here rather than on the
                    // UI thread. Tool payloads can be large, and this commonly
                    // runs many times per frame during a burst.
                    let project_start = std::time::Instant::now();
                    let event = match record["type"].as_str().unwrap_or("") {
                        "message_update" => decode_message_update(&record),
                        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                            decode_tool_event(&record)
                        }
                        _ => None,
                    };
                    // Approval failures are detected by scanning the tool
                    // result, which can be large. Do it here, not on the UI
                    // thread.
                    let approval_error = record["type"] == "tool_execution_end"
                        && record["isError"] == true
                        && record["result"]["content"]
                            .as_array()
                            .is_some_and(|blocks| {
                                blocks.iter().any(|block| {
                                    block["text"].as_str().is_some_and(|text| {
                                        text.contains("requires approval")
                                            || text.contains("no interactive UI")
                                    })
                                })
                            });
                    crate::stream_timing::record_since("bridge.omp.project", project_start);
                    if sender
                        .send(Incoming::Record {
                            record,
                            event,
                            approval_error,
                        })
                        .is_err()
                    {
                        return;
                    }
                    crate::agent_bridge::signal_wake();
                }
            }
            let _ = sender.send(Incoming::Closed);
            crate::agent_bridge::signal_wake();
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
        // Worker messages are emitted separately from the parent transcript.
        let _ = write_command(
            &stdin,
            &json!({"type": "set_subagent_subscription", "id": "kathamo-subagents", "level": "events"}),
        );
        // Learn where this session is stored so its generated title can be read.
        let _ = write_command(&stdin, &json!({"type": "get_state", "id": "kathamo-state"}));
        Ok(Self {
            child,
            stdin,
            events,
            sender: bridge_sender,
            next_id: 0,
            pending_prompt: None,
            closed: false,
            session_file: None,
            title: None,
            title_requested: false,
            title_in_flight: false,
            title_read_in_flight: false,
            live_workers: HashMap::new(),
            worker_changed: false,
        })
    }

    pub fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if self.pending_prompt.is_some() {
            return Err("An OMP prompt is already running".into());
        }
        // A title request is background work, never worth blocking a prompt
        // for. Cancel it and let the next completed turn ask again.
        if self.title_in_flight {
            self.title_in_flight = false;
            self.title_requested = false;
            let _ = self.send(json!({"type": "abort"}));
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

    /// Read the session file header and publish the title if it is new. Used
    /// when OMP has already named the session itself. The read runs on a
    /// background thread; its result arrives as `Incoming::TitleRead`.
    fn publish_session_title(&mut self) {
        if self.title.is_some() || self.title_read_in_flight {
            return;
        }
        let Some(path) = self.session_file.clone() else {
            return;
        };
        self.title_read_in_flight = true;
        let sender = self.sender.clone();
        thread::spawn(move || {
            let start = std::time::Instant::now();
            let title = read_session_title(&path);
            crate::stream_timing::record_since("omp.title_read", start);
            let _ = sender.send(Incoming::TitleRead(title));
            crate::agent_bridge::signal_wake();
        });
    }

    pub fn poll(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(incoming) = self.events.try_recv() {
            match incoming {
                Incoming::Error(error) => events.push(AgentEvent::Error(error)),
                Incoming::TitleRead(title) => {
                    self.title_read_in_flight = false;
                    if let Some(title) = title
                        && self.title.as_deref() != Some(title.as_str())
                    {
                        self.title = Some(title.clone());
                        events.push(AgentEvent::Title(title));
                    }
                }
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
                Incoming::Record {
                    record,
                    event,
                    approval_error,
                } => self.handle_record(&record, event, approval_error, &mut events),
            }
        }
        coalesce_events(events)
    }

    pub fn take_worker_changes(&mut self) -> bool {
        std::mem::take(&mut self.worker_changed)
    }

    pub fn live_worker(
        &self,
        worker: &crate::subagent_chat::Worker,
        tool_id: Option<&str>,
    ) -> Option<&crate::subagent_chat::LiveWorker> {
        let live = self.live_workers.get(&worker.id)?;
        let same_file = live
            .transcript
            .as_ref()
            .zip(worker.transcript.as_ref())
            .is_some_and(|(live, saved)| live == saved);
        let same_call = live
            .tool_id
            .as_deref()
            .zip(tool_id)
            .is_some_and(|(live, saved)| live == saved);
        (same_file || same_call).then_some(live)
    }

    fn handle_record(
        &mut self,
        record: &Value,
        event: Option<AgentEvent>,
        approval_error: bool,
        events: &mut Vec<AgentEvent>,
    ) {
        let matching_prompt = self
            .pending_prompt
            .as_deref()
            .is_some_and(|id| record["id"].as_str() == Some(id));
        match record["type"].as_str().unwrap_or("") {
            "response" if record["command"] == "get_state" => {
                if let Some(path) = record["data"]["sessionFile"].as_str() {
                    self.session_file = Some(PathBuf::from(path));
                }
            }
            // OMP's canonical announcement of a session rename or generated
            // title. This is the event the TUI itself listens to.
            "session_info_update" => {
                if let Some(title) = record["title"].as_str()
                    && self.title.as_deref() != Some(title)
                {
                    self.title = Some(title.to_string());
                    events.push(AgentEvent::Title(title.to_string()));
                }
                // The rename finished, so prompts are accepted again.
                self.title_in_flight = false;
            }
            // A rename that produced no title (e.g. OMP declined a low-signal
            // prompt). End the background request so prompts flow again.
            "command_output" => {
                self.title_in_flight = false;
                if let Some(text) = record["text"].as_str()
                    && let Some(title) = text.strip_prefix("Session renamed to ")
                {
                    let title = title.trim();
                    let title = title.strip_suffix('.').unwrap_or(title).to_string();
                    if !title.is_empty() && self.title.as_deref() != Some(title.as_str()) {
                        self.title = Some(title.clone());
                        events.push(AgentEvent::Title(title));
                    }
                }
            }
            "response" if record["id"] == "kathamo-event-filter" => {}
            "response" if record["success"] == false => {
                let command = record["command"].as_str().unwrap_or("command");
                let detail = record["error"].as_str().unwrap_or("Unknown error");
                // A failed title request must not surface as an agent error or
                // leave prompts permanently blocked.
                if self.title_in_flight && is_busy_rejection(detail) {
                    self.title_in_flight = false;
                } else {
                    events.push(AgentEvent::Error(format!("OMP {command}: {detail}")));
                }
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
                // OMP only names a session when asked. Ask once, after the
                // first completed turn, so the sidebar gets the agent's own
                // title instead of a prompt excerpt.
                if !self.title_requested {
                    self.title_requested = true;
                    self.title_in_flight = true;
                    let _ = self.send(json!({"type": "prompt", "message": "/rename"}));
                }
                // The file header is the fallback when OMP already named it.
                self.publish_session_title();
                events.push(AgentEvent::Done);
            }
            "subagent_lifecycle" if record["payload"]["status"] == "started" => {
                let payload = &record["payload"];
                if let Some(id) = payload["id"].as_str() {
                    let transcript = payload["sessionFile"].as_str().map(PathBuf::from);
                    let tool_id = payload["parentToolCallId"].as_str().map(str::to_owned);
                    let worker = self.live_workers.entry(id.to_owned()).or_default();
                    if transcript != worker.transcript {
                        *worker = Default::default();
                    }
                    worker.transcript = transcript;
                    worker.tool_id = tool_id;
                }
            }
            "subagent_event" => {
                let payload = &record["payload"];
                if let Some(worker) = payload["id"]
                    .as_str()
                    .and_then(|id| self.live_workers.get_mut(id))
                {
                    self.worker_changed |= worker.apply(&payload["event"]);
                }
            }
            "message_update" => {
                if let Some(event) = event {
                    events.push(event);
                }
            }
            "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                if let Some(event) = event {
                    events.push(event);
                }
                if let Some(id) = record["toolCallId"].as_str() {
                    let payload = if record["type"] == "tool_execution_update" {
                        &record["partialResult"]
                    } else {
                        &record["result"]
                    };
                    let workers = crate::subagent_chat::structured(
                        record["toolName"].as_str().unwrap_or(""),
                        &payload["details"],
                        self.session_file.as_deref(),
                    );
                    if !workers.is_empty() {
                        events.push(AgentEvent::Subagents {
                            tool_id: id.to_owned(),
                            workers,
                        });
                    }
                }
                if approval_error {
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

/// OMP records the generated conversation title in the first line of its
/// session file as `{"type":"title","title":"..."}`. It is not part of the RPC
/// event stream, so the file is the only way to learn it.
fn read_session_title(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    parse_title_line(&first)
}

/// Parse the `{"type":"title","title":"..."}` header OMP writes as the first
/// line of a session file. Returns `None` until OMP has generated a title.
fn parse_title_line(line: &str) -> Option<String> {
    let record: Value = serde_json::from_str(line.trim()).ok()?;
    if record["type"] != "title" {
        return None;
    }
    let title = record["title"].as_str()?.trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// True when OMP rejected a prompt because background work (a title request)
/// was still running. That is ours, not the user's, so it must not surface as
/// an agent error.
fn is_busy_rejection(detail: &str) -> bool {
    detail.contains("already processing")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_title_is_read_from_the_header_line() {
        assert_eq!(
            parse_title_line(r#"{"type":"title","v":1,"title":"🔌 PROV: Add All OMP Providers"}"#),
            Some("🔌 PROV: Add All OMP Providers".to_string())
        );
    }

    #[test]
    fn a_title_request_does_not_block_or_error_a_real_prompt() {
        // OMP rejects a prompt while the rename is still running; Kathamo
        // cancels it first, and any such rejection is never shown to the user.
        assert!(is_busy_rejection(
            "Agent is already processing. Use steer() or followUp() to queue messages, or wait for completion."
        ));
        assert!(!is_busy_rejection("No API key configured"));
    }

    #[test]
    fn an_empty_or_missing_title_is_not_reported() {
        // OMP writes a placeholder header before it has named the session.
        assert_eq!(
            parse_title_line(r#"{"type":"title","v":1,"title":""}"#),
            None
        );
        assert_eq!(
            parse_title_line(r#"{"type":"title","v":1,"title":"   "}"#),
            None
        );
        // Anything other than the title header is not a title.
        assert_eq!(parse_title_line(r#"{"type":"session","id":"abc"}"#), None);
        assert_eq!(parse_title_line("not json"), None);
    }
}
