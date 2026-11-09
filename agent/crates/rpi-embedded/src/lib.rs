//! Embeddable rpi sessions for a host UI.
//!
//! The host supplies a working directory plus an optional `provider/model`
//! selection and session id. This crate reuses `rpi-cli`'s startup path — model
//! and credential resolution, built-in tools, system prompt, JSONL session
//! storage and `AgentHarness` — but never spawns the `rpi` executable and never
//! speaks the RPC JSONL protocol.
//!
//! - [`EmbeddedSession`] owns one conversation on a background Tokio runtime.
//!   [`EmbeddedSession::poll`] drains typed [`SessionEvent`]s, so a host can keep
//!   its existing polling UI loop.
//! - [`catalog`] and [`providers`] replace `rpi --list-models` and `rpi auth
//!   check` with in-process reads of the same `models.json`, `auth.json`,
//!   `settings.json` and environment sources.
//! - [`Login`] replaces `rpi auth login` for API-key providers and drives
//!   extension-owned OAuth actions in-process.
//!
//! Sessions run with `no_extensions` so several conversations can coexist in one
//! process without sharing rpi's process-global plugin session identity. Model
//! discovery and login still load extensions when they need an OAuth provider.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use rpi_agent::events::AgentEvent as RpiEvent;
use rpi_agent::message::AgentMessage;
use rpi_ai::types::{AssistantMessageEvent, StopReason};
use rpi_cli::args::{Args, Mode};
use rpi_cli::session::BuildError;
use rpi_harness::agent_harness::AgentLane;
use rpi_runtime::config::Credential;
use rpi_runtime::{ProviderRegistry, RuntimeConfig, RuntimeSettings, ValueSource};

/// One catalog row, matching the identity Kathamo shows in its model picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
}

/// Local provider readiness. No credential material is exposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderState {
    pub id: String,
    pub ready: bool,
    pub source: Option<String>,
}

/// Read the configured model catalog in-process (`models.json` + built-ins).
pub fn catalog() -> Result<Vec<ModelInfo>, String> {
    rpi_cli::provider::catalog_all()
        .map(|models| {
            models
                .into_iter()
                .map(|model| ModelInfo {
                    provider: model.provider,
                    id: model.id,
                })
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// Report local readiness for every provider known to the model/credential
/// stores, using the same rules as execution.
pub fn providers() -> Result<Vec<ProviderState>, String> {
    let config = RuntimeConfig {
        models: rpi_cli::config::load_models_config().map_err(|error| error.to_string())?,
        credentials: rpi_cli::config::read_auth().map_err(|error| error.to_string())?,
        settings: RuntimeSettings::default(),
    };
    let values = EnvironmentValues;
    let states = config.auth_states(&ProviderRegistry::default(), &values);
    Ok(states
        .into_iter()
        .map(|(id, state)| ProviderState {
            id,
            ready: state.is_ready(),
            source: Some(state.source.as_json_str().to_string()),
        })
        .collect())
}

static TOOL_ARRIVAL_HOOK: OnceLock<fn(&str)> = OnceLock::new();

/// Install a host callback invoked when a tool-start event reaches the embedded
/// forwarder. Hosts use it to correlate RPC arrival with the first painted card.
/// It never changes agent behavior.
pub fn set_tool_arrival_hook(hook: fn(&str)) {
    let _ = TOOL_ARRIVAL_HOOK.set(hook);
}

/// Events delivered to the host. This mirrors Kathamo's existing chat event
/// consumer so the subprocess bridge can be replaced without changing the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Text(String),
    Thinking(String),
    Status(String),
    ToolStart {
        id: Option<String>,
        name: String,
        input: String,
    },
    ToolUpdate {
        id: Option<String>,
        name: String,
        output: String,
    },
    ToolEnd {
        id: Option<String>,
        name: String,
        output: String,
        failed: bool,
    },
    Error(String),
    Done,
    Closed,
}

/// Inputs for one embedded conversation.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    /// Provider id from the host's model picker.
    pub provider: Option<String>,
    /// Model id from the host's model picker. May itself contain `/`.
    pub model: Option<String>,
    /// Restore this durable rpi session when present. If it no longer exists,
    /// the worker starts a fresh session instead of failing the pane.
    pub session_id: Option<String>,
}

impl SessionOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            provider: None,
            model: None,
            session_id: None,
        }
    }

    /// Parse the `provider/model` selector Kathamo uses on launch.
    pub fn with_selector(mut self, selector: Option<&str>) -> Self {
        if let Some(selector) = selector {
            match selector.split_once('/') {
                Some((provider, model)) => {
                    self.provider = Some(provider.to_string());
                    self.model = Some(model.to_string());
                }
                None => self.model = Some(selector.to_string()),
            }
        }
        self
    }

    pub fn with_session(mut self, session_id: Option<&str>) -> Self {
        self.session_id = session_id.map(str::to_owned);
        self
    }
}

enum Command {
    Prompt(String),
    Abort,
    Shutdown,
}

/// One in-process agent conversation.
pub struct EmbeddedSession {
    events: mpsc::Receiver<SessionEvent>,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    session_id: Arc<Mutex<Option<String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    closed: bool,
}

impl EmbeddedSession {
    /// Start the session worker. This returns immediately; model resolution and
    /// harness construction happen on the worker, and prompts sent meanwhile are
    /// queued.
    pub fn spawn(options: SessionOptions) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let session_id = Arc::new(Mutex::new(None));
        let worker_session_id = Arc::clone(&session_id);
        let worker = std::thread::Builder::new()
            .name("rpi-embedded".into())
            .spawn(move || {
                run_worker(options, command_rx, event_tx, worker_session_id);
            })
            .map_err(|error| format!("Could not start the embedded agent: {error}"))?;
        Ok(Self {
            events: event_rx,
            commands: command_tx,
            session_id,
            worker: Some(worker),
            closed: false,
        })
    }

    /// The durable session id, once the harness has opened or created one.
    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .ok()
            .and_then(|session| session.clone())
    }

    pub fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if self.closed {
            return Err("The agent session has closed".into());
        }
        self.commands
            .send(Command::Prompt(prompt.to_string()))
            .map_err(|_| "The agent session has closed".to_string())
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        if self.closed {
            return Err("The agent session has closed".into());
        }
        self.commands
            .send(Command::Abort)
            .map_err(|_| "The agent session has closed".to_string())
    }

    /// Drain everything that happened since the last poll.
    pub fn poll(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if matches!(event, SessionEvent::Closed) {
                self.closed = true;
            }
            events.push(event);
        }
        events
    }
}

impl Drop for EmbeddedSession {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            // Give cancellation a moment, then detach rather than freezing the
            // host UI if an extension-owned tool ignores the abort.
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("rpi-embedded-reap".into())
                .spawn(move || {
                    let _ = worker.join();
                    let _ = done_tx.send(());
                })
                .ok();
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

fn run_worker(
    options: SessionOptions,
    commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<SessionEvent>,
    session_id: Arc<Mutex<Option<String>>>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("rpi-embedded-rt")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            send_fatal(
                &events,
                format!("Could not start the embedded agent runtime: {error}"),
            );
            return;
        }
    };
    runtime.block_on(run_session(options, commands, events, session_id));
}

async fn run_session(
    options: SessionOptions,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<SessionEvent>,
    session_id: Arc<Mutex<Option<String>>>,
) {
    let SessionOptions {
        cwd,
        provider,
        model,
        session_id: resume,
    } = options;
    let trusted = project_trusted(&cwd);

    // Resolution reads host-owned files and environment; keep it off the async
    // workers that also serve tool I/O.
    let resolve_cwd = cwd.clone();
    let resolve_provider = provider.clone();
    let resolve_model = model.clone();
    let resolved = match tokio::task::spawn_blocking(move || {
        rpi_cli::provider::resolve_for_cwd(
            resolve_provider.as_deref(),
            resolve_model.as_deref(),
            None,
            None,
            None,
            &resolve_cwd,
            trusted,
        )
    })
    .await
    {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(error)) => {
            send_fatal(&events, error.to_string());
            return;
        }
        Err(error) => {
            send_fatal(
                &events,
                format!("Could not resolve the agent model: {error}"),
            );
            return;
        }
    };

    let mut args = Args::default();
    args.mode = Mode::Rpc;
    // Several embedded conversations share one process. rpi's extension layer
    // publishes a process-global session id, so keep the embedded core on the
    // built-in tool set and its provider runtimes. Login still loads extensions
    // when it needs an OAuth provider.
    args.no_extensions = true;
    args.offline = true;
    args.provider = provider.clone();
    args.model = model.clone();
    args.session = resume.clone();

    let mut built = rpi_cli::session::build(&resolved, &args, &cwd, trusted).await;
    if built.is_err() && resume.is_some() {
        // A stale id must not brick the pane; the host transcript is still on
        // screen, so start a fresh durable session and publish its new id.
        let mut fresh = args.clone();
        fresh.session = None;
        if let Ok(retry) = rpi_cli::session::build(&resolved, &fresh, &cwd, trusted).await {
            built = Ok(retry);
        }
    }

    let (harness, event_rx, reload) = match built {
        Ok(built) => built,
        Err(error) => {
            send_fatal(&events, describe_build_error(error));
            return;
        }
    };
    let harness = Arc::new(harness);
    if let Ok(mut slot) = session_id.lock() {
        *slot = Some(harness.session_id());
    }

    // Forward the harness event bus into the host's polling loop. `reload` keeps
    // extension/resource leases and routers alive for the session lifetime.
    let forward_tx = events.clone();
    let forwarder = tokio::spawn(async move {
        let _reload = reload;
        let mut event_rx = event_rx;
        loop {
            match event_rx.recv().await {
                Ok(event) => forward_event(event, &forward_tx),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    let _ = forward_tx.send(SessionEvent::Status(format!(
                        "The agent dropped {dropped} intermediate updates."
                    )));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let running = Arc::new(AtomicBool::new(false));
    while let Some(command) = commands.recv().await {
        match command {
            Command::Prompt(text) => {
                if running.swap(true, Ordering::SeqCst) {
                    let _ = events.send(SessionEvent::Error(
                        "The agent is already running a prompt".into(),
                    ));
                    continue;
                }
                let lane: Arc<dyn AgentLane> = harness.clone();
                let event_sink = events.clone();
                let run_flag = Arc::clone(&running);
                tokio::spawn(async move {
                    if let Err(error) = lane.prompt_text(&text, Vec::new()).await {
                        let _ = event_sink.send(SessionEvent::Error(error.to_string()));
                    }
                    run_flag.store(false, Ordering::SeqCst);
                    let _ = event_sink.send(SessionEvent::Done);
                });
            }
            Command::Abort => {
                harness.request_stop_round();
                if let Err(error) = harness.abort_reclaiming(true).await {
                    let _ = events.send(SessionEvent::Error(error.to_string()));
                }
                let _ = events.send(SessionEvent::Done);
            }
            Command::Shutdown => {
                harness.request_stop_round();
                let _ = harness.abort_reclaiming(true).await;
                break;
            }
        }
    }

    forwarder.abort();
    let _ = events.send(SessionEvent::Closed);
}

fn project_trusted(cwd: &Path) -> bool {
    rpi_cli::config::project_trust_decision(cwd)
        .ok()
        .flatten()
        .unwrap_or(true)
}

fn describe_build_error(error: BuildError) -> String {
    match error {
        BuildError::SessionNotFound { requested, dir } => format!(
            "The saved agent session {requested} was not found in {dir}. Start a new conversation."
        ),
        other => format!("Could not start the embedded agent: {other}"),
    }
}

fn send_fatal(events: &mpsc::Sender<SessionEvent>, message: String) {
    let _ = events.send(SessionEvent::Error(message));
    let _ = events.send(SessionEvent::Done);
    let _ = events.send(SessionEvent::Closed);
}

fn forward_event(event: RpiEvent, events: &mpsc::Sender<SessionEvent>) {
    match event {
        RpiEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                let _ = events.send(SessionEvent::Text(delta));
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                let _ = events.send(SessionEvent::Thinking(delta));
            }
            _ => {}
        },
        RpiEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            if let Some(hook) = TOOL_ARRIVAL_HOOK.get() {
                hook(&tool_call_id);
            }
            let _ = events.send(SessionEvent::ToolStart {
                id: Some(tool_call_id),
                name: tool_name,
                input: serde_json::to_string(&args).unwrap_or_default(),
            });
        }
        RpiEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            partial_result,
            ..
        } => {
            let _ = events.send(SessionEvent::ToolUpdate {
                id: Some(tool_call_id),
                name: tool_name,
                output: tool_result_text(&partial_result),
            });
        }
        RpiEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            let _ = events.send(SessionEvent::ToolEnd {
                id: Some(tool_call_id),
                name: tool_name,
                output: tool_result_text(&result),
                failed: is_error,
            });
        }
        RpiEvent::MessageEnd { message, .. } => {
            if let AgentMessage::Assistant(assistant) = message {
                if assistant.stop_reason == StopReason::Error {
                    let error = assistant
                        .error_message
                        .unwrap_or_else(|| "The agent encountered an error".into());
                    let _ = events.send(SessionEvent::Error(error));
                }
            }
        }
        RpiEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            error,
        } => {
            let _ = events.send(SessionEvent::Status(format!(
                "Retrying model request (attempt {attempt}/{max_retries}; delay {delay_ms} ms): {error}"
            )));
        }
        RpiEvent::AgentEnd { .. } => {
            let _ = events.send(SessionEvent::Done);
        }
        _ => {}
    }
}

fn tool_result_text(result: &rpi_agent::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            rpi_agent::TextContentOrImage::Text(text) => Some(text.text.as_str()),
            rpi_agent::TextContentOrImage::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct EnvironmentValues;

impl ValueSource for EnvironmentValues {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Result of one in-process authentication step.
#[derive(Clone, Debug)]
pub enum LoginEvent {
    Output(String),
    Finished(Result<(), String>),
}

/// In-process replacement for `rpi auth login`.
///
/// API-key providers read one line through [`Login::submit`] and persist it with
/// the same `auth.json` writer the CLI uses. Providers that registered an OAuth
/// extension are driven through their `begin`/`exchange` actions; display and
/// instruction text is forwarded to the host, while the authorization value is
/// only sent back to the extension. The value is never echoed.
pub struct Login {
    events: mpsc::Receiver<LoginEvent>,
    input: Option<mpsc::Sender<String>>,
    finished: bool,
}

impl Login {
    pub fn start(provider: &str, cwd: &Path) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        let provider = provider.to_string();
        let cwd = cwd.to_path_buf();
        std::thread::Builder::new()
            .name("rpi-embedded-login".into())
            .spawn(move || login_worker(provider, cwd, event_tx, input_rx))
            .map_err(|error| format!("Could not start in-process authentication: {error}"))?;
        Ok(Self {
            events: event_rx,
            input: Some(input_tx),
            finished: false,
        })
    }

    pub fn try_event(&mut self) -> Option<LoginEvent> {
        let event = self.events.try_recv().ok()?;
        if matches!(event, LoginEvent::Finished(_)) {
            self.finished = true;
            self.input = None;
        }
        Some(event)
    }

    pub fn submit(&mut self, input: &str) -> Result<(), String> {
        if self.finished {
            return Err("Authentication has already finished".into());
        }
        if input.contains(['\r', '\n', '\0']) {
            return Err("Authentication input must be a single line".into());
        }
        self.input
            .as_ref()
            .ok_or_else(|| "Authentication is not waiting for input".to_string())?
            .send(input.to_string())
            .map_err(|_| "Authentication has closed".to_string())
    }

    pub fn cancel(&mut self) {
        self.finished = true;
        self.input = None;
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn login_worker(
    provider: String,
    cwd: PathBuf,
    events: mpsc::Sender<LoginEvent>,
    inputs: mpsc::Receiver<String>,
) {
    let trusted = project_trusted(&cwd);
    let prepared = rpi_cli::session::prepare_extensions(&Args::default(), &cwd, trusted).ok();
    let snapshot = prepared
        .as_ref()
        .and_then(|prepared| prepared.extension_session.snapshot_arc());
    let oauth = snapshot
        .as_ref()
        .filter(|snapshot| snapshot.oauth_provider(&provider).is_some());

    if let Some(snapshot) = oauth {
        run_oauth(provider, Arc::clone(snapshot), events, inputs);
    } else {
        run_api_key(provider, events, inputs);
    }
}

fn run_api_key(provider: String, events: mpsc::Sender<LoginEvent>, inputs: mpsc::Receiver<String>) {
    let _ = events.send(LoginEvent::Output(format!(
        "Enter API key for {provider}: "
    )));
    let key = match inputs.recv() {
        Ok(key) => key,
        Err(_) => {
            let _ = events.send(LoginEvent::Finished(Err(
                "Authentication was cancelled".into()
            )));
            return;
        }
    };
    let key = key.trim();
    if key.is_empty() {
        let _ = events.send(LoginEvent::Finished(Err(
            "An empty key was entered; nothing was saved.".into(),
        )));
        return;
    }
    let credential = Credential::ApiKey {
        key: Some(key.to_string()),
        env: None,
    };
    let result = rpi_cli::config::upsert_credential(&provider, credential)
        .map_err(|error| format!("Could not save credentials: {error}"));
    let _ = events.send(LoginEvent::Finished(result));
}

fn run_oauth(
    provider: String,
    snapshot: Arc<rpi_extensions::RegistrySnapshot>,
    events: mpsc::Sender<LoginEvent>,
    inputs: mpsc::Receiver<String>,
) {
    let mut input = serde_json::json!({});
    let mut action = "begin";
    loop {
        let raw = match rpi_extensions::request_oauth(&snapshot, &provider, action, input) {
            Ok(value) => value,
            Err(error) => {
                let _ = events.send(LoginEvent::Finished(Err(format!(
                    "OAuth {action} failed: {error}"
                ))));
                return;
            }
        };
        let response = match rpi_extensions::OAuthActionResponse::parse(raw) {
            Ok(response) => response,
            Err(error) => {
                let _ = events.send(LoginEvent::Finished(Err(format!(
                    "Invalid OAuth response: {error}"
                ))));
                return;
            }
        };
        match response.kind {
            rpi_extensions::OAuthResponseKind::Credential => {
                let token = match response.credential() {
                    Ok(Some(token)) => token,
                    _ => {
                        let _ = events.send(LoginEvent::Finished(Err(
                            "OAuth response contained no valid credential".into(),
                        )));
                        return;
                    }
                };
                let result = rpi_cli::config::upsert_oauth_token(
                    &provider,
                    token.access,
                    token.refresh,
                    token.expires_at,
                )
                .map_err(|error| format!("Could not save OAuth credentials: {error}"));
                let _ = events.send(LoginEvent::Finished(result));
                return;
            }
            rpi_extensions::OAuthResponseKind::Error => {
                let _ = events.send(LoginEvent::Finished(Err(
                    "The OAuth provider rejected the request".into(),
                )));
                return;
            }
            rpi_extensions::OAuthResponseKind::Success => {
                let _ = events.send(LoginEvent::Finished(Err(
                    "OAuth login completed without credentials".into(),
                )));
                return;
            }
            rpi_extensions::OAuthResponseKind::Interaction => {
                let display = response.payload.get("display").unwrap_or(&response.payload);
                let mut text = String::new();
                if let Some(title) = display.get("title").and_then(serde_json::Value::as_str) {
                    text.push_str(title);
                    text.push('\n');
                }
                if let Some(url) = display.get("url").and_then(serde_json::Value::as_str) {
                    text.push_str(url);
                    text.push('\n');
                }
                if let Some(instructions) = display
                    .get("instructions")
                    .and_then(serde_json::Value::as_array)
                {
                    for instruction in instructions.iter().filter_map(serde_json::Value::as_str) {
                        text.push_str(instruction);
                        text.push('\n');
                    }
                }
                text.push_str("OAuth input: ");
                let _ = events.send(LoginEvent::Output(text));
                let value = match inputs.recv() {
                    Ok(value) => value,
                    Err(_) => {
                        let _ = events.send(LoginEvent::Finished(Err(
                            "Authentication was cancelled".into(),
                        )));
                        return;
                    }
                };
                if value.trim().is_empty() {
                    let _ = events.send(LoginEvent::Finished(Err(
                        "Empty OAuth input; nothing was saved.".into(),
                    )));
                    return;
                }
                input = serde_json::json!({ "value": value.trim() });
                action = "exchange";
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_splits_provider_from_a_model_that_contains_slashes() {
        let options = SessionOptions::new("/tmp").with_selector(Some("commandcode/deepseek/v4"));
        assert_eq!(options.provider.as_deref(), Some("commandcode"));
        assert_eq!(options.model.as_deref(), Some("deepseek/v4"));
    }

    #[test]
    fn tool_text_joins_text_blocks_only() {
        let result = rpi_agent::AgentToolResult {
            content: vec![
                rpi_agent::TextContentOrImage::text("one"),
                rpi_agent::TextContentOrImage::text("two"),
            ],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        };
        assert_eq!(tool_result_text(&result), "one\ntwo");
    }

    #[test]
    fn environment_value_source_reads_the_process_environment() {
        std::env::set_var("RPI_EMBEDDED_TEST_VALUE", "ok");
        let values = EnvironmentValues;
        assert_eq!(values.env("RPI_EMBEDDED_TEST_VALUE").as_deref(), Some("ok"));
        assert_eq!(values.env("RPI_EMBEDDED_TEST_MISSING"), None);
        std::env::remove_var("RPI_EMBEDDED_TEST_VALUE");
    }

    #[test]
    fn event_mapping_covers_text_tools_retries_and_completion() {
        let (tx, rx) = mpsc::channel();
        let partial = Arc::new(rpi_ai::AssistantMessage::empty(
            rpi_ai::Api::Faux,
            "faux",
            "m",
            0,
        ));
        forward_event(
            RpiEvent::MessageUpdate {
                message_id: None,
                message: Arc::clone(&partial),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "hello".into(),
                    partial: Arc::clone(&partial),
                },
            },
            &tx,
        );
        forward_event(
            RpiEvent::ToolExecutionStart {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({"command": "printf hi"}),
            },
            &tx,
        );
        let result = rpi_agent::AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("hi")],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        };
        forward_event(
            RpiEvent::ToolExecutionEnd {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                result,
                is_error: false,
            },
            &tx,
        );
        forward_event(
            RpiEvent::RetryScheduled {
                attempt: 1,
                max_retries: 3,
                delay_ms: 250,
                error: "busy".into(),
            },
            &tx,
        );
        forward_event(
            RpiEvent::AgentEnd {
                messages: Vec::new(),
            },
            &tx,
        );
        drop(tx);

        let events = rx.into_iter().collect::<Vec<_>>();
        assert!(matches!(&events[0], SessionEvent::Text(text) if text == "hello"));
        assert!(
            matches!(&events[1], SessionEvent::ToolStart { id, name, input }
            if id.as_deref() == Some("t1") && name == "bash" && input.contains("printf hi"))
        );
        assert!(
            matches!(&events[2], SessionEvent::ToolEnd { output, failed, .. }
            if output == "hi" && !failed)
        );
        assert!(matches!(&events[3], SessionEvent::Status(text) if text.contains("attempt 1/3")));
        assert_eq!(events[4], SessionEvent::Done);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_faux_harness_streams_text_and_completion_through_the_mapping() {
        use rpi_ai::providers::faux::{FauxProvider, FauxScript};
        use rpi_ai::{Api, Model};
        use rpi_harness::agent_harness::AgentLane;
        use rpi_runtime::{ModelRequest, ProviderDeclaration, ProviderRegistry, RuntimeConfig};
        use std::collections::BTreeMap;

        let model = Model::new(
            "embedded-test",
            "Embedded Test",
            Api::Faux,
            "faux",
            "http://localhost:0",
        );
        let mut providers = ProviderRegistry::default();
        providers.declare(ProviderDeclaration {
            id: "faux".into(),
            models: vec![model.clone()],
            api_key_env: None,
            requires_auth: false,
        });
        providers.register(
            FauxProvider::with_models(
                FauxScript::new().with_text("Hello from embedded."),
                vec![model],
            ),
            Api::Faux,
        );
        let resolved = RuntimeConfig::default()
            .resolve(
                &ModelRequest {
                    provider: Some("faux"),
                    model: Some("embedded-test"),
                    ..Default::default()
                },
                &providers,
                &BTreeMap::<String, String>::new(),
            )
            .expect("faux model resolves");

        let dir = tempfile::tempdir().expect("temp dir");
        let mut args = Args::default();
        args.mode = Mode::Rpc;
        args.no_session = true;
        args.no_extensions = true;
        args.offline = true;
        args.no_skills = true;
        args.no_prompt_templates = true;
        args.no_context_files = true;
        let (harness, mut event_rx, _reload) =
            rpi_cli::session::build(&resolved, &args, dir.path(), true)
                .await
                .expect("faux harness builds");
        let lane: Arc<dyn AgentLane> = Arc::new(harness);
        let prompt_lane = Arc::clone(&lane);
        let prompt =
            tokio::spawn(async move { prompt_lane.prompt_text("hello", Vec::new()).await });

        let (mapped_tx, mapped_rx) = mpsc::channel();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, event_rx.recv()).await {
                Ok(Ok(event)) => {
                    let end = matches!(event, RpiEvent::AgentEnd { .. });
                    forward_event(event, &mapped_tx);
                    if end {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = prompt.await.expect("prompt task joins");
        drop(mapped_tx);

        let events = mapped_rx.into_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Text(text) if text.contains("Hello"))),
            "expected streamed text, got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Done)),
            "expected completion, got {events:?}"
        );
    }

    #[test]
    fn a_missing_model_reports_a_fatal_error_and_closes() {
        let mut session = EmbeddedSession::spawn(SessionOptions {
            cwd: std::env::temp_dir(),
            provider: Some("definitely-not-a-provider".into()),
            model: Some("missing-model".into()),
            session_id: None,
        })
        .expect("worker starts");

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline {
            events.extend(session.poll());
            if events
                .iter()
                .any(|event| matches!(event, SessionEvent::Closed))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Error(_))),
            "expected a fatal error, got {events:?}"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::Done)));
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::Closed)));
    }
}
