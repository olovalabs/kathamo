# Streaming performance audit

## Scope and architecture

The existing asynchronous shell executor, cancellation, capture/log spool, PTY
lifecycle and FIFO input writer were preserved. No dependencies were added.
Measurements were made on Linux x86-64, AMD Ryzen 5 5600G (6 cores / 12 threads).
Headless GPUI checks use the real text system and render-to-image path, not a
mock renderer. They do not measure compositor presentation.

| Pipeline stage | Execution and bounds | Audit decision |
|---|---|---|
| Provider SSE / decoding | `rpi-ai` provider transport; immediate first delta, existing 64-byte / 4 ms subsequent batching; bounded 64-event / 4 MiB transport with terminal/result slots | Preserve; no measured provider rewrite justified |
| Agent / durable harness | `rpi-agent`, `rpi-harness`; shared partial snapshots, owned deltas; durable frame recording before host emission | Preserve; use existing agent/frame diagnostics |
| Embedded raw admission | `agent/crates/rpi-embedded/src/lib.rs`; 256 records / 4 MiB; shared tool results and compatible snapshot replacement | Add opt-in admission, lock, residence and backpressure timings |
| Projection / host admission | Background projection into a second 256-record / 4 MiB queue; one oversized final record admitted exclusively | Preserve final payloads and backpressure; measure projection |
| Bridge / GPUI reducer | `src/agent_bridge.rs`, `AppState::poll_agent_events`; 64 records / 256 KiB per pane, explicit backlog resignal, structural-boundary-safe coalescing | Coalesce callback attempts before callback mutex/Arc work; no fixed UI timer |
| Transcript persistence | `src/conversations.rs`; capped fields into existing 256-entry / 8 MiB nonblocking writer queue | Preserve; serialization and filesystem writes remain on writer worker |
| Chat rendering | Virtualized transcript; cached tool arguments and 16 KiB / 400-line output previews | Preserve virtualization and full-output copy/expand behavior; scan only unfinished Markdown boundaries |
| Markdown / highlighting | `components/src/text/text_view.rs` | Move initial parse off GPUI; bound requested/completed documents and publish compatible prefixes monotonically |
| Shell output | `rpi-tools` Tokio stdout/stderr capture; eight-record channel, bounded tail, asynchronous full log, immediate first snapshot and existing 16 ms quiet-period checkpoints | Preserve optimized execution; mark first nonempty host snapshot before raw admission |
| Interactive terminal | Blocking reader worker → 256 chunks of at most 32 KiB → 128 KiB cooperative foreground parse → Alacritty grid → GPUI | Preserve parser/thread ownership; optimize measured snapshot allocations and search painting |

Cancellation, terminal/error records, tool approvals and session ownership retain
their existing paths. No content is intentionally dropped by the new chat
mailboxes: requests contain cumulative documents, and the latest/final document
remains pending even if the producer closes.

### Zed reference

Primary sources inspected:

- [TerminalBuilder::subscribe](https://github.com/zed-industries/zed/blob/main/crates/terminal/src/terminal.rs): immediate first event; subsequent 4 ms batching, coalesced wakeups, cooperative yields.
- [Markdown::parse / start_background_parse](https://github.com/zed-industries/zed/blob/main/crates/markdown/src/markdown.rs): background parsing with one pending parse and a reparse flag; keep previous parsed content while new work completes.

These are architectural references, not an assertion that Kathamo now matches
Zed's compositor frame pacing. A trial fixed 4 ms host-pump timer reduced pump
calls but increased the standalone model's worst queue latency to 20.233 ms; it
was rejected. The existing GPUI Fast Linux dispatcher returns foreground work
through idle dispatch, so replacing its cooperative yield was not justified.

## Confirmed causes and fixes

### Initial Markdown parsing blocked layout

Previously `TextView::request_layout` called `parse_content` synchronously,
including code highlighting. Updates parsed in the background, but their text,
style and completed-tree channels were unbounded.

Now both initial and subsequent parsing use a background worker. Initial
Markdown immediately exposes a selectable native paragraph sharing its source;
HTML waits for parsed content rather than briefly showing raw tags. Capacity-one
wake channels accompany one desired document, one active parse and one completed
result. Old trees are released outside the mailbox lock.

Append-only updates share a document epoch. Completed prefixes may publish while
new tokens arrive, with strictly increasing applied generations. Replacement,
format, theme and parse-affecting style changes invalidate earlier work. This
avoids both stale-document rollback and starvation from rejecting every parse
that finishes during a continuous stream. Fresh render-time heading callbacks
update the current context without starting an endless parse/notify loop.

### Newline tokens rescanned settled responses

`refresh_stream_view` previously rescanned the whole accumulated response on
every newline-bearing delta. It now starts at the previous settled boundary,
retaining unfinished list/fence context. Differential checks covered 21,509
adversarial newline boundaries plus all character chunk sizes of fenced code,
lists, tables, CRLF and Unicode fixtures. Complete text is unchanged.

A standalone exact-scanner workload (4,000 initial paragraphs, 2,000 appended
paragraphs) scanned **370,029,000 → 58,000 bytes**, **240,361 → 80 µs**. These are
scanner timings, not overall UI speedups. The integrated ignored benchmark also
compares the previous cache/copy implementation with the new one in the same
process.

### Every embedded event attempted a host callback

A capacity-one host wake channel already existed, but every event still acquired
the callback mutex, cloned callback references and attempted the callback.
`Wake` now coalesces before that work with a per-session atomic pending flag.
Polling acknowledges before draining and resignals backlog. Installing/replacing
a callback rearms preexisting work. There is no intentional token delay.

Standalone exact-queue/callback measurements, five interleaved runs, 100,000
five-byte deltas per session:

| Scenario | Before median | After median | Before callback attempts | After attempts |
|---|---:|---:|---:|---:|
| One fast consumer | 15.579 ms | 14.956 ms | 100,000 | ~1,540 |
| Four fast consumers | 20.852 ms | 18.959 ms | 400,000 | ~6,090 |
| One slow consumer, 50 µs sleeps | 170.843 ms | 170.426 ms | 100,000 | 1,560 |
| Four slow consumers, 50 µs sleeps | 169.062 ms | 169.109 ms | 400,000 | 6,240 |

This is approximately 98.5% fewer callback attempts, not 98.5% fewer GPUI frames.
The standalone harness stubs Tokio notifications; real network/runtime/UI
behavior is exercised separately by the loopback suite.

### Terminal snapshot layout allocated per cell

`TerminalRenderer::layout_row` constructed temporary strings per cell. It now
appends directly into styled runs while preserving combining characters, wide
cells, styles and decoration ranges. Exact old/new layout output equality was
checked on the benchmark fixtures.

| 120-column × 10,000-row snapshot | Before p95 | After p95 | Before allocation calls, 101 samples | After allocation calls, 101 samples |
|---|---:|---:|---:|---:|
| Plain | 33.831 ms | 22.083 ms | 129,280,000 | 7,070,000 |
| Styled | 38.915 ms | 25.733 ms | 147,460,000 | 26,260,000 |

These are snapshot-layout CPU kernels, not 10,000-row viewport paints. Typical
48-row standalone p95 layout was 0.158 → 0.109 ms plain and 0.185 → 0.121 ms
styled. Allocation counting adds its own overhead.

### Search paint repeatedly scanned every match

Painting 48 visible rows with 5,000 matches visited 240,000 candidates per frame.
Row-local binary ranges reduce the measured indexing kernel p95 from
**188,397 → 2,134 ns**. Multiline/scrollback match boundaries remain covered.
An owed throttled search refresh now has one cancellable timer; previously it
could wait for an unrelated future frame after output stopped. Resize and
visibility invalidations share one refresh.

### Parser ownership was not the measured bottleneck

Actual existing `TerminalState` parsing, 128 KiB batches, 120 × 48 grid, 101
samples after 10 warmups:

| Fixture | Before p50 | Before p95 | Before maximum |
|---|---:|---:|---:|
| Plain | 1.762 ms | 1.963 ms | 2.562 ms |
| ANSI | 1.803 ms | 2.094 ms | 3.320 ms |
| Pathological unbroken line | 1.717 ms | 2.803 ms | 3.394 ms |
| Restoration | 1.543 ms | 1.650 ms | 2.652 ms |

The parser and its bounded cooperative batches were therefore retained instead
of introducing a contended background-grid lock or a new runtime.

## Instrumentation and interpretation

- `KATHAMO_STREAM_TIMINGS=1`: raw/projected admission, lock, backpressure and
  residence; embedded projection; `ui.pump`, drain/apply/render; Markdown parse,
  queue/result wait and superseded-context parse; first reasoning/answer paint;
  `tool.output_to_paint` in addition to existing tool-start-to-output timing.
- `RPI_AGENT_DIAGNOSTICS=1`: existing provider event, fold and emitter timing.
- `RPI_FRAME_DIAGNOSTICS=1`: existing durable-frame queue/append timing.
- `KATHAMO_DIAGNOSTICS=1`: CPU/RSS/context switches, terminal parse bytes/time,
  sampled output queue depth, read-to-parse, read-to-paint and parse-to-paint.

First-token paint metadata includes the turn's start timestamp, preventing old
rows/canvases from marking a later warm turn. Responses that settle before their
first frame remain measurable. Collapsed reasoning does not count as visible
reasoning text.

Tool-output timing starts at the first nonempty host snapshot, **not** the
subprocess stdout read; the existing shell checkpoint interval is outside that
sample. PTY read timestamps begin after a successful read and include bounded
channel wait. Hidden panes retain their oldest pending age until painted.
All paint markers end at CPU command recording, not GPU/compositor scanout.

Telemetry is opt-in and metadata-only. Queue telemetry allocates no timestamp
storage and performs no per-event timing-hook invocation when disabled.
Percentiles retain at most the first 4,096 samples per label/window; count and
maximum cover all observations. Enabling instrumentation perturbs execution.

## Integrated measurements

Baseline used the pre-change test executable already present in
`target/debug/deps/kathamo-80d8014916405007`. After integration the same named
workloads were run serially, without concurrent compilation. These are
observational single-run comparisons, not controlled confidence intervals.
Preliminary runs alongside compilation/tests were discarded as performance
comparisons, not hidden as passing latency results.

| Workload / metric | Before | After |
|---|---:|---:|
| Headless short chat, frame p95, two after runs | 11.049 ms | 9.047–10.297 ms |
| Headless 722-message chat, frame p95, two after runs | 9.846 ms | 9.351–10.985 ms |
| Headless 722-message burst chat, frame p95, two after runs | 10.237 ms | 9.288–9.353 ms |
| Short / long / burst first-delta-to-paint, first serial run | 19.909 / 21.975 / 17.792 ms | 15.418 / 13.705 / 12.838 ms |
| Warm provider first-delta-to-apply p50 | 20 µs | 20 µs |
| Warm local-SSE send-to-first-delta p50 | 1.596 ms | 1.409 ms |
| Cancellation-to-settle | 0.695 ms | 0.683 ms |
| Tool start-to-first-output paint p50 | 10.467 ms | 10.296 ms |
| First nonempty tool snapshot-to-paint p50 / p95 | Not instrumented | 0.402 / 0.489 ms |
| GPUI apply-to-paint, short / long / burst after instrumentation | Not instrumented | 12.506 / 13.567 / 13.718 ms |
| Actual PTY, 20,000 ANSI lines, warmed read-to-paint average / maximum | Not instrumented | 1.32 / 25.54 ms |
| Loopback process user / system CPU | 0.8883 / 0.2895 s | 0.7514 / 0.2220 s |
| Loopback child peak RSS | 189,916 KiB | 188,500 KiB |
| Incremental split plus real cache/copy control, 2,000 paragraphs | 578.640 ms | 7.088 ms |

The last row compares the prior whole-document newline scan and the new scan
in one integrated benchmark process, retaining the same settled/tail copying.
The loopback suite verifies complete text, exactly one completion per turn,
512-token bursts, concurrent sessions, cancellation, backend failure followed by
successful reuse, provider switching, tool results and slow command consumers.
It does not render those provider responses; the separate headless benchmarks
exercise rendering. Cold startup, RSS and CPU include catalog/runtime/cache
effects and are not isolated evidence of a leak fix or provider speedup.

The highlighted-Markdown first-layout control used 135,000 bytes / 1,000
sections: synchronous parse/highlight took **72.225 ms**, versus **1 µs** to
construct the shared native fallback. Parsing still does that work in the
background; the native fallback result excludes text shaping and GPU painting.

The integrated terminal parser benchmark after the change measured 128 KiB
batch p95 of 2.267 ms plain, 2.502 ms ANSI, 2.064 ms unbroken lines and 2.080 ms
restoration. A repeat search-index control in the same binary measured p95
230,717 → 2,054 ns. Absolute standalone/integrated numbers differ with profiles
and system load; no parser throughput improvement is claimed.

The real PTY benchmark runs Bash through the existing input/reader/parser/view
path, verifies its final marker, paints during submission and advances the
headless dispatcher's clock. After eight startup frames and counter reset, it
completed 20,000 ANSI lines in 192 ms: queue-wait maximum 24.83 ms, parse maximum
1.45 ms, paint-command maximum 2.96 ms, and parse-to-paint maximum 3.26 ms.
The three explicit image-capture frames had median 9.485 ms and maximum
11.144 ms. Process CPU was 119.2%, RSS 209.4 MiB. There is no before measurement
for the new arrival markers. An un-warmed run included a 301.31 ms read-to-paint
maximum spanning startup; it is not represented as steady-stream performance.
The remaining 25.54 ms warmed tail is reported, not a claim of universal 60 FPS.
Diagnostic “fps” counts render callbacks, not display refreshes or scanout.

The tool-output paint fixture sets the first-snapshot marker immediately before
applying its synthetic tool update. Its sub-millisecond result isolates GPUI
apply/paint, not subprocess execution or the shell checkpoint interval.

## Verification and reproduction

Executed successfully:

- Application test build: `cargo test --locked --bin kathamo --no-run`.
- Focused chat/input/rendering suite: **47 passed, 9 ignored**. Physical-write
  tests now await the already-existing asynchronous receipt contract; reasoning
  interaction tests expand the already-collapsed header before inspecting its
  body. No shell-execution behavior was changed to satisfy these assertions.
- Terminal library: **136 passed, 3 ignored**; chunked ANSI/Unicode/mode/cursor
  and capture ordering, search boundaries, styled runs, selection and PTY input.
- Embedded library: **17 passed**, including wake installation/replacement,
  reentrant callbacks, 2,000 enqueue/poll races, bounded delivery and final events.
- Local TextView suite: **7 passed, 1 ignored**; bounded updates, continuous
  compatible-prefix publication, stale-context rejection, final HTML delivery,
  callback/style handling and worker shutdown.
- Existing shell integration suites `bash_throttle`,
  `bash_stdout_and_stdin_hold`, `execution_env_conformance`: **16 passed**.
- Provider `event_stream` integration suite: **4 passed**.
- Local SSE runner and ignored chat/tool, partition, Markdown and terminal
  benchmarks passed. Rendered tool output was captured and visually inspected
  at `/tmp/kathamo-streaming-tool.png`.

The initial broader application run was **241 passed, 7 failed, 10 ignored**.
Five affected interaction-test assumptions were corrected as described above and
their focused suite passed. Two unrelated assertions remain intentionally
unchanged: `pane_tests::sidebar_new_session_and_remove_session_icons_align`
(225 px versus 249 px), and `theme::tests::test_theme_tokens_modes`
(sidebar/window background equality). A fully green repository suite is not
claimed.

Useful reproduction commands (test executable hashes depend on the build):

```sh
cargo test --locked --bin kathamo kathamo_bottom_input_tests -- --test-threads=1
cargo test --locked --manifest-path terminal/Cargo.toml --lib
cargo test --locked --manifest-path agent/crates/rpi-embedded/Cargo.toml --lib
cargo test --manifest-path components/Cargo.toml --lib text::text_view::tests
cargo test --locked --manifest-path agent/Cargo.toml -p rpi-tools \
  --test bash_throttle --test bash_stdout_and_stdin_hold --test execution_env_conformance
cargo test --locked --manifest-path agent/Cargo.toml -p rpi-ai --test event_stream
python3 scripts/bench-agent-stream.py target/debug/deps/kathamo-<hash>
target/debug/deps/kathamo-<hash> streaming_chat_frame_benchmark --ignored --nocapture
target/debug/deps/kathamo-<hash> streaming_hot_path_microbench --ignored --nocapture
KATHAMO_DIAGNOSTICS=1 target/debug/deps/kathamo-<hash> terminal_stream_frame_benchmark --ignored --nocapture
KATHAMO_STREAM_TIMINGS=1 target/debug/deps/kathamo-<hash> tool_feedback_frame_benchmark --ignored --nocapture
cargo run --manifest-path terminal/Cargo.toml --release --example bench_streaming -- 10000
```

Root `cargo test -p gpui-component` is ambiguous (0.5.1 and 0.9.0) and a
dependency cannot be tested from that workspace when it needs dev-dependencies.
Use the component's own manifest. Its temporary standalone lockfile is not
shipped. Some Cargo-output wrappers suppress successful `--nocapture` details;
running the printed test executable directly preserves the timing output.

## Changed files

| File | Change |
|---|---|
| `agent/crates/rpi-embedded/src/lib.rs` | Atomic wake coalescing, queue-stage telemetry, first tool snapshot hook, wake/race/backpressure regressions |
| `src/agent_bridge.rs` | First-output timing for decoded bridge events |
| `src/stream_timing.rs` | Output-to-paint timing, turn identity stamp, accurate maxima |
| `src/main.rs` | Incremental boundary scan, generation-safe final paint markers, pump timing, hook installation, benchmarks and interaction test contract updates |
| `src/diagnostics.rs` | Report PTY queue and read/parse-to-paint counters |
| `components/src/text/text_view.rs` | Background initial parse, bounded cumulative mailbox, monotonic/epoch publication, selectable fallback, worker-lifetime and streaming regressions |
| `terminal/src/render.rs` | Per-run snapshot strings, row-local search ranges and layout/search regressions |
| `terminal/src/view.rs` | Output timestamps, cancellable trailing search refresh, coalesced search invalidation |
| `terminal/src/terminal.rs` | ANSI/Unicode/mode/cursor/capture chunk-equivalence regression and parser benchmark |
| `terminal/src/diagnostics.rs` | Bounded metadata-only output latency counters |
| `terminal/examples/bench_streaming.rs` | Reproducible parser/layout workload and allocation counter |
| `ARCHITECTURE.md`, `CHANGELOG.md`, this report | Architecture, evidence, reproduction and limits |

## Remaining limits

- No compositor connection was available: the X socket directory was empty;
  advertised display variables alone do not prove an available display. No
  compositor dropped-frame, GPU-completion or input-to-photon result is claimed.
- No macOS or Windows runtime validation. No live-provider network performance
  claim; deterministic local SSE avoids variable provider load.
- Markdown parsing remains whole-document within each rendered partition; a
  single enormous paragraph/fence still needs native text shaping/layout.
  Bounded pending work is not a bound on one document's size or parse cost.
- The existing terminal control-event `std::sync::mpsc` is unbounded. PTY byte
  backpressure is bounded, but malicious OSC/control-event floods remain a
  separate risk; blocking that channel on the foreground parser would deadlock.
- Full-scrollback search refresh remains foreground work at its existing
  throttled cadence. Snapshot layout is improved but not an incremental rope.
- No total-process allocation census or memory-leak proof is claimed. Allocation
  counts above cover the measured snapshot kernel; RSS is process-level.
- Normal headless frame timing is not a guaranteed 60 FPS production result.
  Compilation/parallel-test contention produced much slower preliminary runs;
  those runs are not evidence of a before/after UI speedup.
