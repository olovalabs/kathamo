use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::thread;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent_bridge::{AgentEvent, coalesce_events, decode_message_update, decode_tool_event};
use crate::agent_writer::AgentWriter;

const WORKER_PROMPT: &str = r#"## Parallel subagents
When delegation is useful, start independent assignments together in one task batch
using the tasks array when available. Use background tasks when supported. Start all
independent workers before waiting for any result. Wait early only when a later
assignment actually depends on an earlier result. Give each worker its own goal
and keep concurrent edits in separate files or isolated workspaces.
"#;

// OMP includes the accumulated worker message and partial snapshot in each
// update. Deserialize just the fields the live transcript actually uses.
#[derive(Deserialize)]
struct WorkerFrame {
    payload: WorkerPayload,
}

#[derive(Deserialize)]
struct WorkerPayload {
    id: String,
    event: WorkerEvent,
}

#[derive(Deserialize)]
struct WorkerEvent {
    #[serde(rename = "type")]
    kind: String,
    message: Option<WorkerMessage>,
    #[serde(rename = "assistantMessageEvent")]
    delta: Option<WorkerDelta>,
}

#[derive(Deserialize)]
struct WorkerMessage {
    timestamp: Option<u64>,
}

#[derive(Deserialize)]
struct WorkerDelta {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<String>,
}

enum Incoming {
    /// Assistant deltas need no other fields from the RPC record.
    Delta(AgentEvent),
    WorkerDelta {
        id: String,
        role: crate::ChatRole,
        text: String,
        timestamp: Option<u64>,
    },
    /// A decoded record with its projected tool lifecycle event, if any.
    Record {
        record: Value,
        event: Option<AgentEvent>,
        /// Precomputed on the reader thread: this failed tool result is an
        /// approval request that this view could not answer.
        approval_error: bool,
        received: std::time::Instant,
    },
    Error(String),
    /// A background read of OMP's session-file title finished.
    TitleRead(Option<String>),
    Closed,
}

impl WorkerPayload {
    fn into_delta(self) -> Option<Incoming> {
        let delta = self.event.delta?;
        let role = match delta.kind.as_str() {
            "text_delta" => crate::ChatRole::Assistant,
            "thinking_delta" => crate::ChatRole::Thinking,
            _ => return None,
        };
        let text = delta.delta.filter(|text| !text.is_empty())?;
        Some(Incoming::WorkerDelta {
            id: self.id,
            role,
            text,
            timestamp: self.event.message.and_then(|message| message.timestamp),
        })
    }
}

/// A separate, persistent OMP conversation for one Google Antigravity model.
pub struct OmpBridge {
    child: Child,
    writer: AgentWriter,
    events: mpsc::Receiver<Incoming>,
    sender: mpsc::Sender<Incoming>,
    next_id: u64,
    trace_id: u64,
    pending_prompt: Option<String>,
    closed: bool,
    /// Where OMP persists this session, learned from `get_state`. OMP writes its
    /// auto-generated conversation title into the first line of that file.
    session_file: Option<PathBuf>,
    title: Option<String>,
    /// Antigravity uses the local first-prompt title to avoid a model request.
    auto_title: bool,
    /// Other providers ask OMP to generate a title after the first turn.
    title_requested: bool,
    /// A title request is running as background work. A prompt that arrives
    /// meanwhile cancels it rather than failing as "already processing".
    title_in_flight: bool,
    /// The session file is being read on a background thread.
    title_read_in_flight: bool,
    live_workers: HashMap<String, crate::subagent_chat::LiveWorker>,
    worker_changed: bool,
    worker_requests: HashMap<String, String>,
}

impl OmpBridge {
    pub fn spawn(cwd: &Path, model: &str, thinking_level: &str) -> Result<Self, String> {
        let model = model.trim();
        if model.is_empty() {
            return Err("No model was selected".into());
        }
        let selector = if model.contains('/') {
            model.to_string()
        } else {
            format!("google-antigravity/{model}")
        };
        let (provider, _) = selector
            .split_once('/')
            .expect("model selector includes provider");
        let system_prompt = format!("{}\n\n{WORKER_PROMPT}", crate::agent_ui::CATALOG_PROMPT);
        let prepare_start = std::time::Instant::now();
        let mut command = crate::provider::runtime::command()?;
        crate::provider::auth::storage::sync_api_key(provider)?;
        command
            .args(["--mode", "rpc", "--model", &selector])
            .args(["--append-system-prompt", &system_prompt])
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
        crate::stream_timing::record("engine.omp.prepare", spawn_start - prepare_start);
        let mut child = command.spawn().map_err(|error| {
            format!(
                "Could not start the Kathamo provider in {}: {error}.",
                cwd.display()
            )
        })?;
        crate::stream_timing::record_since("engine.omp.spawn", spawn_start);
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, events) = mpsc::channel();
        let error_sender = sender.clone();
        let writer = AgentWriter::spawn(stdin, move |error| {
            let _ = error_sender.send(Incoming::Error(error));
            crate::agent_bridge::signal_wake();
        });
        let bridge_sender = sender.clone();
        let reply_writer = writer.clone();
        let trace_id = crate::stream_timing::new_trace();
        let reader_start = std::time::Instant::now();
        let timing_enabled = crate::stream_timing::enabled();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut cache_warming = None;
            let mut line = String::new();
            let mut first_record = true;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Err(error) => {
                        let _ = sender.send(Incoming::Error(format!(
                            "Failed reading Kathamo provider output: {error}"
                        )));
                        break;
                    }
                    Ok(_) => {}
                }
                let parse_start = std::time::Instant::now();
                // OMP serializes the frame's discriminator first. Other
                // layouts still work through the generic decoder below.
                if line.starts_with(r#"{"type":"subagent_event","#) {
                    crate::stream_timing::omp_worker_frame(line.len());
                    if let Ok(WorkerFrame { payload }) = serde_json::from_str(&line)
                        && payload.event.kind == "message_update"
                    {
                        crate::stream_timing::record_since(
                            "bridge.omp.worker_update_decode",
                            parse_start,
                        );
                        if let Some(update) = payload.into_delta() {
                            if sender.send(update).is_err() {
                                return;
                            }
                            crate::agent_bridge::signal_wake();
                        }
                        continue;
                    }
                }
                let record: Value = match serde_json::from_str(&line) {
                    Ok(record) => record,
                    Err(_) => {
                        let _ = sender.send(Incoming::Error(
                            "Invalid Kathamo provider RPC output".into(),
                        ));
                        crate::agent_bridge::signal_wake();
                        continue;
                    }
                };
                crate::stream_timing::record_since("bridge.omp.decode", parse_start);
                if first_record {
                    first_record = false;
                    crate::stream_timing::record_since("engine.omp.first_record", reader_start);
                }
                match record["type"].as_str() {
                    Some("cache_warming_start") => {
                        cache_warming = Some(parse_start);
                        continue;
                    }
                    Some("cache_warming_end") => {
                        if let Some(start) = cache_warming.take() {
                            crate::stream_timing::record(
                                "engine.omp.cache_warming",
                                parse_start.saturating_duration_since(start),
                            );
                        }
                        continue;
                    }
                    _ => {}
                }
                if record["type"] == "message_update" {
                    if let Some(event) = decode_message_update(&record) {
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
                        if sender.send(Incoming::Delta(event)).is_err() {
                            return;
                        }
                        crate::agent_bridge::signal_wake();
                    }
                    continue;
                }
                if record["type"] == "extension_ui_request" {
                    match record["method"].as_str().unwrap_or("") {
                        "select" | "confirm" | "input" | "editor" | "ask" => {
                            // Never approve a request that this UI cannot show or answer.
                            let _ = sender.send(Incoming::Error(
                                "The Kathamo provider requested an interactive dialog or tool approval, which is not supported in this view; the request was cancelled.".into(),
                            ));
                            crate::agent_bridge::signal_wake();
                            if let Err(error) = reply_writer.send_json(json!({
                                "type": "extension_ui_response", "id": record["id"], "cancelled": true
                            })) {
                                let _ = sender.send(Incoming::Error(error));
                                crate::agent_bridge::signal_wake();
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
                                        received: parse_start,
                                    }
                                };
                                let _ = sender.send(event);
                                crate::agent_bridge::signal_wake();
                            }
                        }
                        _ => {}
                    }
                } else {
                    // Project tool payloads on the reader thread.
                    let project_start = std::time::Instant::now();
                    let event = match record["type"].as_str().unwrap_or("") {
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
                            received: parse_start,
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
        let mut session_events = vec![
            "message_update",
            "tool_execution_start",
            "tool_execution_update",
            "tool_execution_end",
            "auto_retry_start",
            "auto_retry_end",
            "auto_compaction_start",
            "auto_compaction_end",
            "session_info_update",
            "command_output",
        ];
        if timing_enabled {
            session_events.extend(["cache_warming_start", "cache_warming_end"]);
        }
        writer.send_json(json!({
            "type": "set_event_filter", "id": "kathamo-event-filter",
            "events": session_events, "messageUpdates": "delta"
        }))?;
        let _ = writer.send_json(json!({
            "type": "set_cache_warming", "id": "kathamo-cache-warming", "mode": "idle"
        }));
        let _ = writer.send_json(json!({
            "type": "set_thinking_level", "id": "kathamo-thinking-level", "level": thinking_level
        }));
        // Worker messages are emitted separately from the parent transcript.
        let _ = writer.send_json(json!({"type": "set_subagent_subscription", "id": "kathamo-subagents", "level": "events"}));
        // Learn where this session is stored so its generated title can be read.
        let _ = writer.send_json(json!({"type": "get_state", "id": "kathamo-state"}));
        Ok(Self::new(
            child,
            writer,
            events,
            bridge_sender,
            trace_id,
            provider,
        ))
    }

    fn new(
        child: Child,
        writer: AgentWriter,
        events: mpsc::Receiver<Incoming>,
        sender: mpsc::Sender<Incoming>,
        trace_id: u64,
        provider: &str,
    ) -> Self {
        Self {
            child,
            writer,
            events,
            sender,
            next_id: 0,
            trace_id,
            pending_prompt: None,
            closed: false,
            session_file: None,
            title: None,
            auto_title: provider != "google-antigravity",
            title_requested: false,
            title_in_flight: false,
            title_read_in_flight: false,
            live_workers: HashMap::new(),
            worker_changed: false,
            worker_requests: HashMap::new(),
        }
    }

    pub fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if self.pending_prompt.is_some() {
            return Err("A Kathamo provider prompt is already running".into());
        }
        // A title request is background work, never worth blocking a prompt
        // for. Cancel it and let the next completed turn ask again.
        if self.title_in_flight {
            self.title_in_flight = false;
            self.title_requested = false;
            let _ = self.send(json!({"type": "abort"}));
        }
        if self.closed {
            return Err("The Kathamo provider process has exited".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = format!("kathamo-{}", self.next_id);
        self.writer.send_prompt(id.clone(), prompt.to_owned())?;
        self.pending_prompt = Some(id);
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        self.send(json!({"type": "abort"})).map(|_| ())
    }

    pub fn trace_id(&self) -> u64 {
        self.trace_id
    }
    pub fn set_thinking_level(&mut self, level: &str) -> Result<(), String> {
        self.send(json!({"type": "set_thinking_level", "level": level}))
            .map(|_| ())
    }

    fn send(&mut self, mut command: Value) -> Result<String, String> {
        if self.closed {
            return Err("The Kathamo provider process has exited".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = format!("kathamo-{}", self.next_id);
        command["id"] = json!(id);
        self.writer.send_json(command)?;
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
        // Leave a frame opportunity when several workers produce a burst.
        for _ in 0..256 {
            let Ok(incoming) = self.events.try_recv() else {
                return coalesce_events(events);
            };
            match incoming {
                Incoming::Error(error) => events.push(AgentEvent::Error(error)),
                Incoming::Delta(event) => events.push(event),
                Incoming::WorkerDelta {
                    id,
                    role,
                    text,
                    timestamp,
                } => {
                    self.worker_changed |= self.live_workers.entry(id).or_default().apply_delta(
                        role,
                        Some(&text),
                        timestamp,
                    );
                }
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
                        self.writer.close();
                        let status = self
                            .child
                            .try_wait()
                            .ok()
                            .flatten()
                            .map(|status| format!(" ({status})"))
                            .unwrap_or_default();
                        events.push(AgentEvent::Error(format!("Kathamo provider process exited{status}. Check your provider login in /auth.")));
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
                    received,
                } => self.handle_record(&record, event, approval_error, received, &mut events),
            }
        }
        crate::agent_bridge::signal_wake();
        coalesce_events(events)
    }

    pub fn steer_worker(&mut self, id: &str, prompt: &str) -> Result<(), String> {
        if !self
            .live_workers
            .get(id)
            .is_some_and(|worker| worker.active)
        {
            return Err("The subagent is no longer active.".into());
        }
        let request =
            self.send(json!({"type": "steer_subagent", "subagentId": id, "message": prompt}))?;
        self.worker_requests.insert(request, id.to_owned());
        Ok(())
    }

    pub fn cancel_worker(&mut self, id: &str) -> Result<(), String> {
        if !self
            .live_workers
            .get(id)
            .is_some_and(|worker| worker.active)
        {
            return Err("The subagent is no longer active.".into());
        }
        let request = self.send(json!({"type": "cancel_subagent", "subagentId": id}))?;
        self.worker_requests.insert(request, id.to_owned());
        Ok(())
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
        received: std::time::Instant,
        events: &mut Vec<AgentEvent>,
    ) {
        if record["type"] == "response"
            && let Some(worker) = record["id"]
                .as_str()
                .and_then(|id| self.worker_requests.remove(id))
                .and_then(|id| self.live_workers.get_mut(&id))
        {
            if record["success"] == false || record["data"]["cancelled"] == false {
                let error = record["error"]
                    .as_str()
                    .unwrap_or("The subagent is no longer active.");
                worker.report_error(error);
                self.worker_changed = true;
            }
            return;
        }
        let matching_prompt = self
            .pending_prompt
            .as_deref()
            .is_some_and(|id| record["id"].as_str() == Some(id));
        if matching_prompt
            && record["type"] == "response"
            && record["command"] == "prompt"
            && record["success"] == true
            && record["data"]["agentInvoked"] != false
        {
            crate::stream_timing::omp_prompt_admitted(self.trace_id, received);
        }
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
                    events.push(AgentEvent::Error(format!(
                        "Kathamo provider {command}: {detail}"
                    )));
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
                        .unwrap_or("Check your Kathamo provider configuration and login.");
                    events.push(AgentEvent::Error(format!(
                        "Kathamo provider request failed: {detail}"
                    )));
                }
                self.pending_prompt = None;
                // OMP only names a session when asked. Ask once, after the
                // first completed turn, so the sidebar gets the agent's own
                // title instead of a prompt excerpt.
                if self.auto_title && !self.title_requested {
                    self.title_requested = true;
                    self.title_in_flight = true;
                    let _ = self.send(json!({"type": "prompt", "message": "/rename"}));
                }
                // The file header is the fallback when OMP already named it.
                self.publish_session_title();
                events.push(AgentEvent::Done);
            }
            "subagent_lifecycle" | "subagent_progress" | "subagent_event" => {
                self.worker_changed |= apply_worker_frame(&mut self.live_workers, record, events);
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
                    events.push(AgentEvent::Error(
                        "The Kathamo provider could not obtain tool approval in this view.".into(),
                    ));
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
                    "A Kathamo provider extension failed during this session.".into(),
                ));
            }
            "rpc_frame_error" => {
                events.push(AgentEvent::Error(
                    "The Kathamo provider could not deliver an RPC event because it exceeded the transport limit."
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
        // The child is gone, so a writer blocked on a full pipe has unwound.
        self.writer.shutdown();
    }
}

fn apply_worker_frame(
    workers: &mut HashMap<String, crate::subagent_chat::LiveWorker>,
    record: &Value,
    events: &mut Vec<AgentEvent>,
) -> bool {
    let payload = &record["payload"];
    let progress = &payload["progress"];
    let Some(id) = payload["id"].as_str().or_else(|| progress["id"].as_str()) else {
        return false;
    };
    let worker = workers.entry(id.to_owned()).or_default();
    if record["type"] == "subagent_event" {
        return worker.apply(&payload["event"]);
    }
    if let Some(transcript) = payload["sessionFile"].as_str() {
        let transcript = PathBuf::from(transcript);
        if worker
            .transcript
            .as_ref()
            .is_some_and(|old| old != &transcript)
        {
            *worker = Default::default();
        }
        worker.transcript = Some(transcript);
    }
    if let Some(tool_id) = payload["parentToolCallId"].as_str() {
        worker.tool_id = Some(tool_id.to_owned());
    }
    let status = match payload["status"]
        .as_str()
        .or_else(|| progress["status"].as_str())
    {
        Some("started") => "running",
        Some(status) => status,
        None => return false,
    };
    if record["type"] == "subagent_lifecycle" {
        worker.active = status == "running";
        if !worker.active {
            worker.finish();
        }
    }
    if record["type"] == "subagent_lifecycle" && status == "running" && worker.tool_id.is_none() {
        worker.tool_id = Some(format!("subagent:{id}"));
    }
    let Some(tool_id) = &worker.tool_id else {
        return false;
    };
    let snapshot = crate::subagent_chat::Worker {
        id: id.to_owned(),
        title: progress["label"].as_str().unwrap_or(id).to_owned(),
        status: status.to_owned(),
        transcript: worker.transcript.clone(),
        result: None,
    };
    if worker.reported.as_ref() == Some(&snapshot) {
        return false;
    }
    worker.reported = Some(snapshot.clone());
    events.push(AgentEvent::Subagents {
        tool_id: tool_id.clone(),
        workers: vec![snapshot],
    });
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_worker_updates_preserve_delta_and_timestamp() {
        let snapshot = "already streamed ".repeat(4_096);
        let frame = json!({
            "payload": {
                "id": "worker",
                "event": {
                    "type": "message_update",
                    "message": {"timestamp": 42, "content": [{"text": snapshot}]},
                    "assistantMessageEvent": {
                        "type": "thinking_delta",
                        "delta": "next\nline",
                        "partial": {"content": [{"thinking": snapshot}]}
                    }
                }
            }
        });
        let frame: WorkerFrame = serde_json::from_str(&frame.to_string()).unwrap();
        assert_eq!(frame.payload.event.kind, "message_update");
        let Some(Incoming::WorkerDelta {
            id,
            role,
            text,
            timestamp,
        }) = frame.payload.into_delta()
        else {
            panic!("worker delta missing");
        };
        let mut worker = crate::subagent_chat::LiveWorker::default();
        assert!(worker.apply_delta(role, Some(&text), timestamp));
        let message = worker.messages_with_history(&[]).next().unwrap();
        assert_eq!(id, "worker");
        assert_eq!(message.text, "next\nline");
        assert_eq!(message.source_timestamp, Some(42));
        assert!(message.streaming);
    }

    #[test]
    fn independent_workers_publish_rows_and_stream_before_either_finishes() {
        use crate::subagent_chat::{LiveWorker, rows};
        let mut workers = HashMap::<String, LiveWorker>::new();
        let mut events = Vec::new();
        for id in ["Alpha", "Bravo"] {
            assert!(apply_worker_frame(
                &mut workers,
                &json!({
                    "type": "subagent_lifecycle",
                    "payload": {"id": id, "parentToolCallId": "task-batch", "status": "started"}
                }),
                &mut events
            ));
            assert!(apply_worker_frame(
                &mut workers,
                &json!({
                    "type": "subagent_event",
                    "payload": {"id": id, "event": {
                        "type": "message_update",
                        "assistantMessageEvent": {"type": "text_delta", "delta": id}
                    }}
                }),
                &mut events
            ));
        }
        let mut messages = Vec::new();
        for event in events.drain(..) {
            crate::push_chat_event(&mut messages, event);
        }
        crate::push_chat_event(
            &mut messages,
            AgentEvent::ToolStart {
                id: Some("task-batch".into()),
                name: "task".into(),
                input: "{}".into(),
            },
        );
        assert_eq!(
            messages.len(),
            1,
            "the actual tool start reuses its worker row"
        );
        assert_eq!(
            rows(&messages)
                .iter()
                .map(|row| row.worker.status_label())
                .collect::<Vec<_>>(),
            ["Running", "Running"]
        );
        for id in ["Alpha", "Bravo"] {
            let message = workers[id].messages_with_history(&[]).next().unwrap();
            assert_eq!(message.text, id);
            assert!(message.streaming);
        }

        assert!(apply_worker_frame(
            &mut workers,
            &json!({
                "type": "subagent_lifecycle",
                "payload": {"id": "Alpha", "parentToolCallId": "task-batch", "status": "completed"}
            }),
            &mut events
        ));
        for event in events {
            crate::push_chat_event(&mut messages, event);
        }
        let rows = rows(&messages);
        assert_eq!(rows[0].worker.status_label(), "Completed");
        assert_eq!(rows[1].worker.status_label(), "Running");
        assert!(!workers["Alpha"].active);
        assert!(workers["Bravo"].active);
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_uses_the_prompt_title_and_allows_manual_rename() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;

        for provider in ["google-antigravity", "xai-oauth"] {
            let mut child = std::process::Command::new("cat")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let writer = AgentWriter::spawn(child.stdin.take().unwrap(), |_| {});
            let (sender, receiver) = mpsc::channel();
            let mut bridge = OmpBridge::new(child, writer, receiver, sender, 0, provider);
            let mut chat = crate::PaneChat::default();
            chat.set_prompt_title("First prompt".into());
            bridge.send_prompt("First prompt").unwrap();
            let mut events = Vec::new();
            bridge.handle_record(
                &json!({"type":"prompt_result","id":"kathamo-1","status":"completed"}),
                None,
                false,
                std::time::Instant::now(),
                &mut events,
            );
            assert!(matches!(events.as_slice(), [AgentEvent::Done]));
            assert_eq!(chat.conversation_title(), Some("First prompt"));

            if provider == "google-antigravity" {
                bridge.send_prompt("/rename Explicit title").unwrap();
                bridge.handle_record(
                    &json!({"type":"session_info_update","title":"Explicit title"}),
                    None,
                    false,
                    std::time::Instant::now(),
                    &mut events,
                );
                let Some(AgentEvent::Title(title)) = events.pop() else {
                    panic!("expected the explicit title");
                };
                chat.set_agent_title(title);
                assert_eq!(chat.conversation_title(), Some("Explicit title"));
            }

            bridge.writer.shutdown();
            let mut output = String::new();
            stdout.read_to_string(&mut output).unwrap();
            let commands: Vec<Value> = output
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(commands.len(), 2);
            assert_eq!(
                commands[1]["message"],
                if provider == "google-antigravity" {
                    "/rename Explicit title"
                } else {
                    "/rename"
                }
            );
        }
    }

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
