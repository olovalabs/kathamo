//! Offline fixture staged by bench-pipeline.py with baseline/current modules.
#![allow(dead_code)]
pub use rpi_ai::AiError;
pub use rpi_harness::{error, messages, session};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
mod baseline_context;
mod baseline_state;
mod prepared_request;
pub mod protocol;
mod remote {
    pub use crate::protocol;
}
mod rpc_events;
use rpi_agent::{AgentEmitter, AgentEvent};
use rpi_ai::{Api, AssistantMessage, AssistantMessageEvent, Content};
use rpi_harness::session::types::*;
struct Counter;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Counter = Counter;
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(p, l, size) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size, Ordering::Relaxed);
        }
        p
    }
}
fn entry(index: usize) -> Entry {
    Entry::Message(MessageEntry {
        base: EntryBase {
            entry_type: "message".into(),
            id: format!("e{index}"),
            seq: index as u64 + 1,
            parent_id: (index > 0).then(|| format!("e{}", index - 1)),
            timestamp: 42,
        },
        message: rpi_ai::UserMessage::new("x".repeat(8192), 42).into(),
        terminate: None,
    })
}
fn measured(name: &str, variant: &str, iterations: usize, mut f: impl FnMut()) {
    f();
    let a = ALLOCS.load(Ordering::Relaxed);
    let b = BYTES.load(Ordering::Relaxed);
    let now = std::time::Instant::now();
    for _ in 0..iterations {
        f()
    }
    let elapsed = now.elapsed().as_micros();
    let count = ALLOCS.load(Ordering::Relaxed) - a;
    let bytes = BYTES.load(Ordering::Relaxed) - b;
    println!(
        "{}",
        serde_json::json!({"case":name,"variant":variant,"iterations":iterations,"elapsed_us":elapsed,"allocations":count,"allocated_bytes":bytes})
    );
}
fn context(variant: &str) {
    let path: Vec<_> = (0..96).map(entry).collect();
    measured("context_builder", variant, 50, || {
        if variant == "before" {
            std::hint::black_box(baseline_context::build_session_context(
                &path,
                &Default::default(),
            ));
        } else {
            std::hint::black_box(session::context::build_session_context(
                &path,
                &Default::default(),
            ));
        }
    });
}
fn state(variant: &str) {
    let entries: Vec<_> = (0..96).map(entry).collect();
    let query = EntryQuery {
        limit: Some(1),
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    };
    measured("session_state", variant, 25, || {
        if variant == "before" {
            let mut s = baseline_state::SessionState::new();
            for e in &entries {
                s.apply_mutation(SessionMutation::Entry {
                    seq: e.seq(),
                    timestamp: 42,
                    lane: Some("main".into()),
                    entry: e.clone(),
                })
                .unwrap();
            }
            std::hint::black_box(
                s.find_entries_on_branch(&query, &Default::default(), "e95")
                    .unwrap(),
            );
        } else {
            let mut s = session::SessionState::new();
            for e in &entries {
                s.apply_mutation(SessionMutation::Entry {
                    seq: e.seq(),
                    timestamp: 42,
                    lane: Some("main".into()),
                    entry: e.clone(),
                })
                .unwrap();
            }
            std::hint::black_box(
                s.find_entries_on_branch(&query, &Default::default(), "e95")
                    .unwrap(),
            );
        }
    });
}
fn http(variant: &str) {
    let client = reqwest::Client::new();
    let body = serde_json::json!({"messages":vec!["x".repeat(8192);64]});
    measured("request_four_attempts", variant, 25, || {
        if variant == "before" {
            for _ in 0..4 {
                let copy = body.clone();
                std::hint::black_box(
                    client
                        .post("http://localhost/test")
                        .json(&copy)
                        .build()
                        .unwrap(),
                );
            }
        } else {
            let p = prepared_request::PreparedJsonRequest::new(
                client.post("http://localhost/test"),
                &body,
            )
            .unwrap();
            for _ in 0..4 {
                std::hint::black_box(prepared_request::clone_attempt(&p));
            }
        }
    });
}
fn rpc(variant: &str) {
    let mut m = AssistantMessage::empty(Api::Faux, "faux", "m", 42);
    m.content.push(Content::text("x".repeat(65536)));
    let m = Arc::new(m);
    let e = AgentEvent::MessageUpdate {
        message: Arc::clone(&m),
        assistant_message_event: AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "x".repeat(64),
            partial: m,
        },
    };
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&rpc_events::encode(&e).unwrap()).unwrap(),
        rpc_events::project(&e)
    );
    measured("rpc_update_64k", variant, 100, || {
        if variant == "before" {
            std::hint::black_box(rpc_events::project(&e).to_string().into_bytes());
        } else {
            std::hint::black_box(rpc_events::encode(&e).unwrap());
        }
    });
}
async fn progress_run(volatile: bool, events: &[AgentEvent]) {
    use session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
    let s = session::Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "bench".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )),
        None,
    );
    let s = if volatile {
        s.with_volatile_progress()
    } else {
        s
    };
    let rec = rpi_harness::frame_progress::FrameRecordingEmitter::new(
        Arc::new(rpi_agent::CollectorEmitter::default()),
        s,
        "main",
        "run",
    );
    for e in events {
        rec.emit(e.clone()).await;
    }
    rec.clear().await.unwrap();
}
fn progress(variant: &str) {
    let mut m = AssistantMessage::empty(Api::Faux, "faux", "m", 42);
    let mut events = vec![AgentEvent::MessageStart {
        message: m.clone().into(),
    }];
    m.content.push(Content::text(""));
    let partial = Arc::new(m.clone());
    events.push(AgentEvent::MessageUpdate {
        message: Arc::clone(&partial),
        assistant_message_event: AssistantMessageEvent::TextStart {
            content_index: 0,
            partial,
        },
    });
    for _ in 0..128 {
        if let Content::Text(t) = &mut m.content[0] {
            t.text.push_str(&"x".repeat(128));
        }
        let partial = Arc::new(m.clone());
        events.push(AgentEvent::MessageUpdate {
            message: Arc::clone(&partial),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".repeat(128),
                partial,
            },
        });
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    measured("progress_128_frames", variant, 20, || {
        rt.block_on(progress_run(variant == "after", &events))
    });
}
fn file(variant: &str) {
    use rpi_tools::FileSystem;
    let temp = tempfile::tempdir().unwrap();
    let env = rpi_tools::OsExecutionEnv::with_cwd(temp.path().to_path_buf());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let bytes = b"{\"text\":\"acknowledged\"}\n";
    measured("file_128_acknowledged_appends", variant, 3, || {
        rt.block_on(async {
            if variant == "before" {
                for _ in 0..128 {
                    env.append_file(
                        "bench.jsonl",
                        rpi_tools::env::FileContent::Bytes(bytes.to_vec()),
                        None,
                    )
                    .await
                    .unwrap();
                }
            } else {
                let mut writer = env.open_append_file("bench.jsonl").await.unwrap().unwrap();
                for _ in 0..128 {
                    writer.append(bytes).await.unwrap();
                }
            }
        })
    });
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let variant = &args[2];
    match args[1].as_str() {
        "context" => context(variant),
        "state" => state(variant),
        "http" => http(variant),
        "rpc" => rpc(variant),
        "progress" => progress(variant),
        "file" => file(variant),
        _ => panic!("unknown case"),
    }
}
