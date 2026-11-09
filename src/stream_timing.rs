//! Opt-in, metadata-only timings for the agent streaming pipeline.
//!
//! Enabled with `KATHAMO_STREAM_TIMINGS=1`. When disabled every entry point is
//! one relaxed atomic load, so the hot path is unchanged. Enabled, it records
//! only durations and event counts — never message text, tool arguments or
//! tool output — and prints a percentile summary every few seconds.
//!
//! The labels are deliberately split by stage so a slow frame can be attributed
//! without a profiler:
//!
//! - `engine.*`       child process spawn and first RPC record
//! - `bridge.*`       JSON decode and event projection on the reader thread
//! - `ui.drain`       draining a bridge's channel on the UI thread
//! - `transcript.*`   serializing and enqueueing a history entry
//! - `ui.apply`       applying one event to the visible transcript
//! - `ui.render_rows` building chat rows in a frame
//! - `markdown.*`     initial and update parses (components crate)
//! - `tool.*`         RPC arrival → UI apply / first card / first output

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const REPORT_INTERVAL_MS: u64 = 5_000;
const MAX_SAMPLES: usize = 4_096;
const MAX_TRACKED_TOOLS: usize = 2_048;

static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("KATHAMO_STREAM_TIMINGS").is_some_and(|value| value != "0"));

/// `true` when stream timings are on. Cheap enough to call per event.
#[inline]
pub fn enabled() -> bool {
    *ENABLED
}

#[derive(Default)]
struct Stats {
    count: u64,
    samples: Vec<u64>,
}

static STATS: LazyLock<Mutex<HashMap<&'static str, Stats>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REPORT_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
static NEXT_REPORT_MS: AtomicU64 = AtomicU64::new(0);

fn with_stats<R>(f: impl FnOnce(&mut HashMap<&'static str, Stats>) -> R) -> R {
    let mut stats = STATS.lock().unwrap_or_else(|error| error.into_inner());
    f(&mut stats)
}

/// Record one duration sample under `label`.
pub fn record(label: &'static str, elapsed: Duration) {
    if !enabled() {
        return;
    }
    let micros = elapsed.as_micros().min(u128::from(u32::MAX)) as u64;
    with_stats(|stats| {
        let entry = stats.entry(label).or_default();
        entry.count += 1;
        if entry.samples.len() < MAX_SAMPLES {
            entry.samples.push(micros);
        }
    });
}

/// Record the time from `start` until now.
#[inline]
pub fn record_since(label: &'static str, start: Instant) {
    if enabled() {
        record(label, start.elapsed());
    }
}

/// Print and reset counters at most every `REPORT_INTERVAL_MS`. Call from an
/// existing periodic task; it is a no-op when disabled or not yet due.
pub fn maybe_report() {
    if !enabled() {
        return;
    }
    let elapsed = REPORT_ORIGIN.elapsed().as_millis() as u64;
    let next = NEXT_REPORT_MS.load(Ordering::Relaxed);
    if next == 0 {
        NEXT_REPORT_MS.store(elapsed + REPORT_INTERVAL_MS, Ordering::Relaxed);
        return;
    }
    if elapsed < next {
        return;
    }
    NEXT_REPORT_MS.store(elapsed + REPORT_INTERVAL_MS, Ordering::Relaxed);
    report();
}

fn report() {
    let mut entries: Vec<(&'static str, Stats)> = with_stats(|stats| stats.drain().collect());
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.1.count));
    eprintln!("kathamo stream timings (us, per report window):");
    for (label, mut stats) in entries {
        stats.samples.sort_unstable();
        let percentile = |fraction: f64| -> u64 {
            if stats.samples.is_empty() {
                return 0;
            }
            let index =
                ((stats.samples.len() as f64 * fraction) as usize).min(stats.samples.len() - 1);
            stats.samples[index]
        };
        let max = stats.samples.last().copied().unwrap_or(0);
        eprintln!(
            "  {label:<26} n={:<6} p50={:<8} p95={:<8} max={}",
            stats.count,
            percentile(0.50),
            percentile(0.95),
            max,
        );
    }
}

struct ToolTrace {
    arrived: Instant,
    card_painted: bool,
    output_painted: bool,
}

static TOOLS: LazyLock<Mutex<HashMap<String, ToolTrace>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Note when a tool start record was decoded. Keyed by the RPC tool id.
pub fn tool_arrived(id: &str) {
    if !enabled() {
        return;
    }
    let mut tools = lock(&TOOLS);
    if tools.len() >= MAX_TRACKED_TOOLS {
        tools.clear();
    }
    tools.insert(
        id.to_owned(),
        ToolTrace {
            arrived: Instant::now(),
            card_painted: false,
            output_painted: false,
        },
    );
}

/// RPC arrival → the event was applied to the visible transcript.
pub fn tool_applied(id: &str) {
    if !enabled() {
        return;
    }
    let arrived = lock(&TOOLS).get(id).map(|trace| trace.arrived);
    if let Some(arrived) = arrived {
        record_since("tool.arrival_to_apply", arrived);
    }
}

/// RPC arrival → the tool card first entered a frame.
pub fn tool_card_painted(id: &str) {
    tool_painted(id, "tool.arrival_to_card", |trace| &mut trace.card_painted);
}

/// RPC arrival → the first non-empty output entered a frame.
pub fn tool_output_painted(id: &str) {
    tool_painted(id, "tool.arrival_to_output", |trace| {
        &mut trace.output_painted
    });
}

fn tool_painted(id: &str, label: &'static str, flag: impl FnOnce(&mut ToolTrace) -> &mut bool) {
    if !enabled() {
        return;
    }
    let arrived = lock(&TOOLS).get_mut(id).and_then(|trace| {
        if std::mem::replace(flag(trace), true) {
            None
        } else {
            Some(trace.arrived)
        }
    });
    if let Some(arrived) = arrived {
        record_since(label, arrived);
    }
}
