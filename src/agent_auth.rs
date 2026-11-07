//! Provider discovery and interactive authentication through the installed CLIs.
//!
//! rpi handles its own providers; Google Antigravity uses omp's OAuth store
//! and login flow. Both interactive commands run in a controlling terminal.

use std::collections::BTreeSet;
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

#[derive(Deserialize)]
struct Status {
    provider: String,
    ready: bool,
    source: Option<String>,
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
    let mut models: Vec<_> = rpi_models(cwd)?
        .into_iter()
        .filter(|(provider, _)| provider != "google-antigravity")
        .map(|(provider, id)| AgentModel {
            provider,
            id,
            backend: ModelBackend::Rpi,
        })
        .collect();
    models.extend(
        optional_antigravity_models(cwd)?
            .unwrap_or_default()
            .into_iter()
            .map(|id| AgentModel {
                provider: "google-antigravity".into(),
                id,
                backend: ModelBackend::Omp,
            }),
    );
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    models.dedup();
    Ok(models)
}

/// Discover catalog and configured provider IDs, then ask rpi for their local
/// authentication status. This performs blocking IO.
pub fn providers(cwd: &Path) -> Result<Vec<Provider>, String> {
    let mut ids: BTreeSet<String> = rpi_models(cwd)?
        .into_iter()
        .map(|(provider, _)| provider)
        .collect();
    ids.extend(configured_providers()?);
    // This provider uses omp even if rpi also advertises the same identifier.
    ids.remove("google-antigravity");
    let mut result = Vec::with_capacity(ids.len() + 1);
    for id in ids {
        let output = Command::new("rpi")
            .args(["auth", "check", "--provider", id.as_str(), "--json"])
            .current_dir(cwd)
            .output()
            .map_err(|e| format!("Could not check authentication for {id}: {e}"))?;
        // Exit status 1 is rpi's normal not-ready result, not an error.
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(format!(
                "Could not check authentication for {id}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let status: Status = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("Could not read authentication status for {id}: {e}"))?;
        if status.provider != id {
            return Err(format!(
                "rpi returned authentication status for the wrong provider: {id}"
            ));
        }
        result.push(Provider {
            id: status.provider,
            ready: status.ready,
            source: status.source,
        });
    }
    result.push(antigravity_provider(cwd));
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}

fn antigravity_provider(cwd: &Path) -> Provider {
    // --list asks omp's actual credential store (including a configured broker)
    // without retrieving tokens. Account identities are never shown in the UI.
    let output = Command::new("omp")
        .args(["token", "google-antigravity", "--list"])
        .current_dir(cwd)
        .output();
    let (ready, source) = match output {
        Ok(output) if output.status.success() => (true, "omp OAuth"),
        Ok(output)
            if output.status.code() == Some(1)
                && String::from_utf8_lossy(&output.stderr)
                    .contains("No OAuth accounts found for provider") =>
        {
            (false, "omp OAuth")
        }
        Ok(_) => (false, "omp authentication status unavailable"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, "omp not installed"),
        Err(_) => (false, "omp authentication status unavailable"),
    };
    Provider {
        id: "google-antigravity".into(),
        ready,
        source: Some(source.into()),
    }
}

fn optional_antigravity_models(cwd: &Path) -> Result<Option<Vec<String>>, String> {
    #[derive(Deserialize)]
    struct Catalog {
        models: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        provider: String,
        kind: String,
        id: String,
    }

    let output = match Command::new("omp")
        .args(["models", "google-antigravity", "--json"])
        .current_dir(cwd)
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Could not run omp to list Antigravity models: {e}")),
    };
    if !output.status.success() {
        return Err(format!(
            "Could not load Antigravity models from omp ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let catalog: Catalog = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Could not read Antigravity models from omp: {e}"))?;
    Ok(Some(
        catalog
            .models
            .into_iter()
            .filter(|model| model.provider == "google-antigravity" && model.kind == "chat")
            .map(|model| model.id)
            .collect(),
    ))
}

fn configured_providers() -> Result<Vec<String>, String> {
    let dir = match std::env::var_os("RPI_CODING_AGENT_DIR") {
        Some(value) => {
            let path = std::path::PathBuf::from(value);
            if !path.is_absolute() {
                return Err("RPI_CODING_AGENT_DIR must be an absolute path".into());
            }
            path
        }
        None => directories::BaseDirs::new()
            .ok_or("Could not locate the rpi configuration directory")?
            .home_dir()
            .join(".rpi")
            .join("agent"),
    };
    let path = dir.join("models.json");
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("Could not read rpi models.json: {error}")),
    };
    let json: serde_json::Value =
        jsonc_parser::parse_to_serde_value(&text, &jsonc_parser::ParseOptions::default())
            .map_err(|_| "Could not parse rpi models.json".to_string())?;
    Ok(json
        .get("providers")
        .and_then(serde_json::Value::as_object)
        .map(|providers| providers.keys().cloned().collect())
        .unwrap_or_default())
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
        let backend = if provider == "google-antigravity" {
            "omp"
        } else {
            "rpi"
        };
        let mut command = CommandBuilder::new(backend);
        if backend == "omp" {
            command.args(["login", "google-antigravity"]);
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
