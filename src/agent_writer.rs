//! Ordered, non-blocking writes to a child-process agent.
//!
//! OMP and rpi RPC run as child processes. Their prompt, abort and extension
//! replies share one stdin pipe, so a single writer thread owns that pipe and
//! preserves command order. Sending only enqueues an owned command, so a large
//! prompt written to a slow-reading child can never block the UI thread.
//!
//! Backpressure is explicit: aborts are always accepted, other JSON commands
//! are rejected past [`MAX_QUEUED_COMMANDS`], prompts past
//! [`MAX_QUEUED_BYTES`], and a broken pipe marks the writer closed, reports one
//! error, and fails later sends immediately.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;
const MAX_QUEUED_COMMANDS: usize = 4_096;

pub(crate) enum WriteCommand {
    /// `{"type":"prompt","message":<text>,"id":<id>}` built on the writer thread.
    Prompt {
        id: String,
        text: String,
    },
    /// A fully-built RPC command.
    Json(serde_json::Value),
    Shutdown,
}

struct Shared {
    sender: mpsc::Sender<WriteCommand>,
    queued_bytes: AtomicUsize,
    queued_commands: AtomicUsize,
    closed: AtomicBool,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone)]
pub(crate) struct AgentWriter {
    shared: Arc<Shared>,
}

impl AgentWriter {
    /// Start a writer thread that owns `sink`. `on_error` runs once on the
    /// writer thread if a write fails (including a broken pipe).
    pub(crate) fn spawn(
        mut sink: impl Write + Send + 'static,
        on_error: impl Fn(String) + Send + 'static,
    ) -> Self {
        let (sender, commands) = mpsc::channel();
        let shared = Arc::new(Shared {
            sender,
            queued_bytes: AtomicUsize::new(0),
            queued_commands: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let writer_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("agent-stdin".into())
            .spawn(move || {
                while let Ok(command) = commands.recv() {
                    let (value, size) = match command {
                        WriteCommand::Prompt { id, text } => {
                            let size = text.len();
                            (
                                serde_json::json!({
                                    "type": "prompt",
                                    "message": text,
                                    "id": id,
                                }),
                                size,
                            )
                        }
                        WriteCommand::Json(value) => (value, usize::MAX),
                        WriteCommand::Shutdown => break,
                    };
                    let result = match serde_json::to_writer(&mut sink, &value) {
                        Ok(()) => sink.write_all(b"\n").and_then(|()| sink.flush()),
                        Err(error) => Err(std::io::Error::other(error)),
                    };
                    if size == usize::MAX {
                        writer_shared
                            .queued_commands
                            .fetch_sub(1, Ordering::Relaxed);
                    } else if size > 0 {
                        writer_shared
                            .queued_bytes
                            .fetch_sub(size, Ordering::Relaxed);
                    }
                    if let Err(error) = result {
                        writer_shared.closed.store(true, Ordering::Release);
                        on_error(format!("Failed to send agent command: {error}"));
                        break;
                    }
                }
            })
            .expect("agent writer thread");
        if let Ok(mut slot) = shared.thread.lock() {
            *slot = Some(thread);
        }
        Self { shared }
    }

    pub(crate) fn send_json(&self, command: serde_json::Value) -> Result<(), String> {
        if self.is_closed() {
            return Err("The agent process has exited".into());
        }
        let urgent = command.get("type").and_then(|kind| kind.as_str()) == Some("abort");
        if !urgent
            && self.shared.queued_commands.fetch_add(1, Ordering::Relaxed) >= MAX_QUEUED_COMMANDS
        {
            self.shared.queued_commands.fetch_sub(1, Ordering::Relaxed);
            return Err("The agent input queue is full; wait for the current command".into());
        }
        if urgent {
            self.shared.queued_commands.fetch_add(1, Ordering::Relaxed);
        }
        if self
            .shared
            .sender
            .send(WriteCommand::Json(command))
            .is_err()
        {
            self.shared.queued_commands.fetch_sub(1, Ordering::Relaxed);
            return Err("The agent process has exited".into());
        }
        Ok(())
    }

    /// Queue a prompt. The prompt text is moved, never serialized on the caller.
    pub(crate) fn send_prompt(&self, id: String, text: String) -> Result<(), String> {
        if self.is_closed() {
            return Err("The agent process has exited".into());
        }
        let size = text.len();
        let queued = self.shared.queued_bytes.fetch_add(size, Ordering::Relaxed);
        if queued.saturating_add(size) > MAX_QUEUED_BYTES {
            self.shared.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            return Err(
                "The agent input queue is full; wait for the current turn to finish".into(),
            );
        }
        if self
            .shared
            .sender
            .send(WriteCommand::Prompt { id, text })
            .is_err()
        {
            self.shared.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            return Err("The agent process has exited".into());
        }
        Ok(())
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Mark the writer closed (for example when the reader observed EOF).
    pub(crate) fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
    }

    /// Close the queue, wait for the writer to notice, and join it. Call after
    /// the child has been killed so a write blocked on backpressure returns.
    pub(crate) fn shutdown(&self) {
        self.close();
        let _ = self.shared.sender.send(WriteCommand::Shutdown);
        let handle = self
            .shared
            .thread
            .lock()
            .ok()
            .and_then(|mut thread| thread.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct SlowSink {
        bytes: Arc<Mutex<Vec<u8>>>,
        delay: Duration,
        fail: bool,
        release: Option<Arc<AtomicBool>>,
    }

    impl SlowSink {
        fn fast(bytes: Arc<Mutex<Vec<u8>>>, delay: Duration) -> Self {
            Self {
                bytes,
                delay,
                fail: false,
                release: None,
            }
        }
    }

    impl Write for SlowSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(release) = &self.release {
                while !release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            thread::sleep(self.delay);
            if self.fail {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ));
            }
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn large_prompts_enqueue_without_waiting_for_a_slow_reader() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = AgentWriter::spawn(
            SlowSink::fast(Arc::clone(&bytes), Duration::from_millis(2)),
            |_| {},
        );

        let start = Instant::now();
        for index in 0..200 {
            writer
                .send_json(serde_json::json!({"type":"abort","id":index}))
                .unwrap();
        }
        let prompt = "x".repeat(2 * 1024 * 1024);
        writer.send_prompt("kathamo-large".into(), prompt).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "enqueue blocked for {:?}",
            start.elapsed()
        );
        writer.shutdown();

        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 201);
        assert!(lines[0].contains("\"id\":0"));
        assert!(lines[199].contains("\"id\":199"));
        assert!(lines[200].contains("\"type\":\"prompt\""));
    }

    #[test]
    fn a_broken_pipe_fails_once_and_then_rejects_sends() {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink_errors = Arc::clone(&errors);
        let writer = AgentWriter::spawn(
            SlowSink {
                bytes: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::ZERO,
                fail: true,
                release: None,
            },
            move |error| sink_errors.lock().unwrap().push(error),
        );
        writer
            .send_json(serde_json::json!({"type":"abort"}))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while errors.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(errors.lock().unwrap().len(), 1, "one reported failure");
        assert!(writer.is_closed());
        assert!(
            writer
                .send_json(serde_json::json!({"type":"abort"}))
                .is_err(),
            "a closed writer rejects new commands"
        );
        writer.shutdown();
    }

    #[test]
    fn prompt_queue_backpressure_is_reported_not_blocking() {
        let release = Arc::new(AtomicBool::new(false));
        let writer = AgentWriter::spawn(
            SlowSink {
                bytes: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::ZERO,
                fail: false,
                release: Some(Arc::clone(&release)),
            },
            |_| {},
        );
        // The first prompt fills most of the queue and parks the writer; a
        // second one is rejected immediately instead of blocking or growing
        // without bound.
        writer
            .send_prompt("a".into(), "x".repeat(MAX_QUEUED_BYTES - 1))
            .unwrap();
        let start = Instant::now();
        assert!(writer.send_prompt("b".into(), "y".repeat(2)).is_err());
        assert!(start.elapsed() < Duration::from_millis(50));
        release.store(true, Ordering::Release);
        writer.shutdown();
    }
}
