//! Provider discovery and interactive authentication through the installed CLIs.
//!
//! rpi handles its own providers; Google Antigravity uses omp's OAuth store
//! and login flow. Both interactive commands run in a controlling terminal.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub ready: bool,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelBackend {
    Rpi,
    Omp,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentModel {
    pub provider: String,
    pub id: String,
    pub backend: ModelBackend,
}


/// Read the rpi model catalog. This performs blocking IO.
fn rpi_models(cwd: &Path) -> Result<Vec<(String, String)>, String> {
    let output = Command::new("rpi")
        .args(["--offline", "--list-models"])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("Could not run rpi: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not load the rpi model catalog: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let warnings = String::from_utf8_lossy(&output.stderr);
    if warnings.contains("could not load models.json") {
        return Err(format!(
            "Could not load the rpi model catalog: {}",
            warnings.trim()
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "The rpi model catalog was not UTF-8".to_string())?;
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    if !header.split_whitespace().take(2).eq(["provider", "model"]) {
        return Err(format!(
            "Could not read the rpi model catalog: {}",
            header.trim()
        ));
    }
    lines
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let provider = fields
                .next()
                .ok_or_else(|| "Missing provider in rpi model catalog".to_string())?;
            let model = fields.next().ok_or_else(|| {
                format!("Missing model in rpi model catalog row: {}", line.trim())
            })?;
            Ok((provider.to_owned(), model.to_owned()))
        })
        .collect()
}

/// Discover models from rpi and omp, keeping Antigravity on its omp backend.
/// This performs blocking IO; a missing omp omits its catalog.
pub fn available_models(cwd: &Path) -> Result<Vec<AgentModel>, String> {
    let mut models: Vec<AgentModel> = Vec::new();

    // 1. Models from rpi (if rpi is installed and succeeds)
    if let Ok(rpi_list) = rpi_models(cwd) {
        for (provider, id) in rpi_list {
            if crate::provider::get_provider(&provider).is_none() {
                models.push(AgentModel {
                    provider,
                    id,
                    backend: ModelBackend::Rpi,
                });
            }
        }
    }

    // 2. All 103 providers from omp catalog
    for (spec, model) in crate::provider::all_models() {
        models.push(AgentModel {
            provider: spec.id.clone(),
            id: model.id.clone(),
            backend: ModelBackend::Omp,
        });
    }

    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    models.dedup();
    Ok(models)
}

/// The model OMP would use by default, from `modelRoles.default` in its config.
/// Only providers Kathamo can drive through OMP are returned.
/// This performs blocking file IO.
pub fn omp_default_model() -> Option<AgentModel> {
    let selector = omp_default_selector()?;
    let (provider, model) = selector.split_once('/')?;
    let id = strip_thinking_suffix(model);
    if id.is_empty() {
        return None;
    }
    Some(AgentModel {
        provider: provider.to_owned(),
        id: id.to_owned(),
        backend: ModelBackend::Omp,
    })
}

fn omp_default_selector() -> Option<String> {
    let path = omp_config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    model_roles_default(&text)
}

fn omp_config_path() -> Option<std::path::PathBuf> {
    if let Some(value) = std::env::var_os("PI_CODING_AGENT_DIR") {
        let path = std::path::PathBuf::from(value);
        if path.is_absolute() {
            return Some(path.join("config.yml"));
        }
    }
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".omp").join("agent").join("config.yml"))
}

/// Extract `modelRoles.default` from an OMP config.yml without pulling in a
/// YAML parser for one scalar.
fn model_roles_default(text: &str) -> Option<String> {
    let mut in_roles = false;
    let mut roles_indent = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if !in_roles {
            if trimmed.starts_with("modelRoles:") {
                in_roles = true;
                roles_indent = indent;
            }
            continue;
        }
        if indent <= roles_indent {
            break;
        }
        let Some(value) = trimmed.strip_prefix("default:") else {
            continue;
        };
        let value = value.split(" #").next().unwrap_or(value).trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        return Some(value.to_owned());
    }
    None
}

/// OMP selectors may carry a thinking level (`provider/model:high`).
fn strip_thinking_suffix(model: &str) -> &str {
    let Some((id, suffix)) = model.rsplit_once(':') else {
        return model;
    };
    if matches!(
        suffix,
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "auto"
    ) {
        id
    } else {
        model
    }
}

/// Discover catalog and configured provider IDs, then ask rpi for their local
/// authentication status. This performs blocking IO.
pub fn providers(cwd: &Path) -> Result<Vec<Provider>, String> {
    crate::provider::restore_stored_credentials();

    let mut result = Vec::new();
    let all_specs = crate::provider::all_providers();
    let active_omp = crate::provider::auth::storage::omp_authenticated_providers();

    for spec in all_specs {
        let status = crate::provider::auth::check_auth_with_active(&spec.id, &active_omp);
        result.push(Provider {
            id: spec.id.clone(),
            ready: status.ready,
            source: status.source,
        });
    }

    // Also include any extra custom providers from credentials.json
    let local_creds = crate::provider::auth::storage::load_local_credentials();
    for (id, cred) in local_creds {
        let source_str = match cred {
            crate::provider::types::StoredCredential::OAuth { .. } => {
                Some("Stored OAuth token".to_string())
            }
            crate::provider::types::StoredCredential::ApiKey { source, .. } => {
                Some(source.unwrap_or_else(|| "Stored API key".to_string()))
            }
        };
        if let Some(existing) = result.iter_mut().find(|p| p.id == id) {
            existing.ready = true;
            if existing.source.is_none() {
                existing.source = source_str;
            }
        } else {
            result.push(Provider {
                id,
                ready: true,
                source: source_str,
            });
        }
    }

    // Also include any extra custom providers from rpi if present
    if let Ok(rpi_ids) = rpi_models(cwd) {
        for (id, _) in rpi_ids {
            if !result.iter().any(|p| p.id == id) {
                result.push(Provider {
                    id,
                    ready: true,
                    source: Some("rpi".to_string()),
                });
            }
        }
    }

    result.sort_by(|a, b| a.id.cmp(&b.id));
    result.dedup_by(|a, b| a.id == b.id);
    Ok(result)
}


pub enum LoginEvent {
    Output(String),
    Finished(Result<(), String>),
}

/// Owns an interactive login. Dropping it cancels and reaps the child.
pub struct Login {
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    events: mpsc::Receiver<LoginEvent>,
    secrets: Arc<Mutex<Vec<Vec<u8>>>>,
    finished: bool,
}

impl Login {
    pub fn start(provider: &str, cwd: &Path) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Could not open an authentication terminal: {e}"))?;
        disable_echo(&*pair.master)?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("Could not read the authentication terminal: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("Could not write to the authentication terminal: {e}"))?;
        let is_omp_provider = crate::provider::get_provider(provider).is_some();
        let backend = if is_omp_provider {
            "omp"
        } else {
            "rpi"
        };
        let mut command = CommandBuilder::new(backend);
        if backend == "omp" {
            command.args(["login", provider]);
        } else {
            command.args(["auth", "login", "--provider", provider]);
        }
        command.cwd(cwd);
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("Could not start {backend} authentication: {e}"))?;
        let killer = child.clone_killer();
        drop(pair.slave);
        drop(pair.master);

        let (tx, events) = mpsc::channel();
        let secrets = Arc::new(Mutex::new(Vec::new()));
        let reader_secrets = Arc::clone(&secrets);
        let output_tx = tx.clone();
        let reader_thread = thread::spawn(move || read_output(reader, reader_secrets, output_tx));
        thread::spawn(move || {
            let status = child.wait();
            let _ = reader_thread.join();
            let result = match status {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(format!("{backend} authentication failed ({status})")),
                Err(e) => Err(format!("Could not wait for {backend} authentication: {e}")),
            };
            let _ = tx.send(LoginEvent::Finished(result));
        });
        Ok(Self {
            writer,
            killer,
            events,
            secrets,
            finished: false,
        })
    }

    pub fn try_event(&mut self) -> Option<LoginEvent> {
        let event = self.events.try_recv().ok()?;
        if matches!(event, LoginEvent::Finished(_)) {
            self.finished = true;
        }
        Some(event)
    }

    /// Send a response to the terminal, never via command line or environment.
    pub fn submit(&mut self, input: &str) -> Result<(), String> {
        if self.finished {
            return Err("Authentication has already finished".into());
        }
        if input.contains(['\r', '\n', '\0']) {
            return Err("Authentication input must be a single line".into());
        }
        if !input.is_empty() {
            self.secrets
                .lock()
                .map_err(|_| "Authentication state is unavailable".to_string())?
                .push(input.as_bytes().to_vec());
        }
        self.writer
            .write_all(input.as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .map_err(|e| format!("Could not send authentication input: {e}"))
    }

    pub fn cancel(&mut self) {
        if !self.finished {
            let _ = self.killer.kill();
            self.finished = true;
        }
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(unix)]
fn disable_echo(master: &dyn portable_pty::MasterPty) -> Result<(), String> {
    let fd = master
        .as_raw_fd()
        .ok_or("Authentication terminal has no file descriptor")?;
    // The settings belong to the PTY, not Kathamo's own terminal. Set them
    // before starting the child so even OAuth's ordinary stdin reads cannot echo.
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) } != 0 {
        return Err(format!(
            "Could not inspect authentication terminal: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut settings = unsafe { settings.assume_init() };
    settings.c_lflag &= !(libc::ECHO | libc::ECHONL);
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &settings) } != 0 {
        return Err(format!(
            "Could not secure authentication terminal: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn disable_echo(_: &dyn portable_pty::MasterPty) -> Result<(), String> {
    Err("Secure authentication terminals are not supported on this platform".into())
}

// In addition to disabling terminal echo, redact submitted responses in output
// from provider extensions. Retain partial matches across read boundaries.
fn read_output(
    mut reader: Box<dyn Read + Send>,
    secrets: Arc<Mutex<Vec<Vec<u8>>>>,
    tx: mpsc::Sender<LoginEvent>,
) {
    let mut buf = [0; 4096];
    let mut pending = Vec::new();
    loop {
        let count = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..count]);
        let Ok(secrets) = secrets.lock() else { break };
        let mut safe = Vec::new();
        while !pending.is_empty() {
            if let Some(secret) = secrets.iter().find(|secret| pending.starts_with(secret)) {
                pending.drain(..secret.len());
                safe.extend_from_slice(b"[redacted]");
            } else if secrets.iter().any(|secret| secret.starts_with(&pending)) {
                break;
            } else {
                safe.push(pending.remove(0));
            }
        }
        drop(secrets);
        if !safe.is_empty()
            && tx
                .send(LoginEvent::Output(
                    String::from_utf8_lossy(&safe).into_owned(),
                ))
                .is_err()
        {
            return;
        }
    }
    // A trailing partial match is deliberately discarded.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_omp_default_model_role() {
        let config = "\
symbolPreset: unicode
modelRoles:
  smol: google-antigravity/gemini-2.5-flash
  default: \"google-antigravity/gemini-3.8-flash:high\"
  plan: openai-codex/gpt-5.5
theme:
  dark: titanium
";
        assert_eq!(
            model_roles_default(config).as_deref(),
            Some("google-antigravity/gemini-3.8-flash:high")
        );
        assert_eq!(model_roles_default("theme:\n  dark: titanium\n"), None);
        assert_eq!(
            model_roles_default("modelRoles:\n  default: google-antigravity/x:high # note\n")
                .as_deref(),
            Some("google-antigravity/x:high")
        );
    }

    #[test]
    fn strips_only_known_thinking_suffixes() {
        assert_eq!(
            strip_thinking_suffix("gemini-3.8-flash:high"),
            "gemini-3.8-flash"
        );
        assert_eq!(
            strip_thinking_suffix("gemini-3.8-flash"),
            "gemini-3.8-flash"
        );
        assert_eq!(
            strip_thinking_suffix("provider/model:v2"),
            "provider/model:v2"
        );
    }

    #[test]
    fn discovers_all_omp_providers() {
        let list = providers(&std::path::PathBuf::from(".")).expect("providers discovery must succeed");
        assert!(list.len() >= 103, "Must discover all 103 omp providers, found: {}", list.len());
        let ids: Vec<&str> = list.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"anthropic"));
        assert!(ids.contains(&"openai-codex"));
        assert!(ids.contains(&"google-antigravity"));
        assert!(ids.contains(&"github-copilot"));
        assert!(ids.contains(&"deepseek"));
        assert!(ids.contains(&"xai-oauth"));
    }

    #[test]
    fn discovers_all_omp_models() {
        let models = available_models(&std::path::PathBuf::from(".")).expect("models discovery must succeed");
        assert!(models.len() > 1000, "Must discover thousands of models, found: {}", models.len());
    }
}
