# Agent streaming latency: implementation report

This change set keeps the engine boundaries the same (rpi 0.21.0 and OMP 18.18.2
still own model parsing, tool dispatch, shell launch and truncation) and removes
Kathamo-side work from the streaming hot path.

## What changed, by finding

### 1. Transcript I/O is no longer on the UI thread

`src/conversations.rs` now serializes an entry and enqueues it to one global
writer thread (`writer_loop` / `write_line`). The writer owns directory
creation, rotation, file handles and `flush`. Because there is one ordered
queue:

- per-conversation append order is preserved;
- the JSONL format and `MAX_FILE_BYTES` rotation policy are unchanged;
- `Log::flush` is a real barrier used by shutdown;
- `clear_all` is a barrier behind every queued append, so deleted history
  cannot be recreated by an in-flight write;
- `Drop` releases the worker's file handle (`WriteJob::Close`) so long sessions
  do not accumulate descriptors.

### 2. OMP projection and title reads moved off the UI thread

`src/omp_bridge.rs` now decodes `message_update` and tool lifecycle records on
the reader thread, including the approval-error scan over the tool result. The
UI thread only applies the projected `AgentEvent`. OMP's session-file title read
also runs on a short-lived background thread and arrives as
`Incoming::TitleRead`, so the foreground never opens or reads that file.

### 3. Streaming markdown split is cached

`ChatMessage` carries `StreamView { settled, tail }`. `refresh_stream_view` is
called when a delta arrives, and rescans the document only when the delta
contains a newline (block boundaries only change at line breaks). Rendering
clones two `SharedString`s instead of rescanning and copying the whole message
every frame. Completion drops the split and returns to the existing full-document
parse semantics (`finish_chat_streaming`).

Thinking collapse now runs once when assistant text starts (`starting_assistant`)
instead of on every token, and `finish_chat_streaming` inspects only the tail
message. Tool events finalize streaming *before* they push their card, preserving
the old invariant with the cheaper tail-only check.

### 4. Tool render fields are parsed once, and output is shared

`ToolDisplay` is built when a tool starts (`ToolDisplay::new`) and caches:

- the header (`ToolView`),
- the shell command,
- file-write content,
- bounded previews of both.

`ChatTool.output` is a `SharedString`. Copy closures and cards clone the `Arc`
instead of copying the full result. `output_preview_len` computes the bounded
view when output arrives; rendering only slices the cached prefix. Outputs over
16 KiB or 400 lines render a preview with **Show all output**, and the copy
action still receives the complete result.

### 5. Bursts are coalesced before the UI and the transcript

`coalesce_events` in `src/agent_bridge.rs` merges adjacent `Text`/`Thinking`
deltas (capped at 16 KiB so the 64 KiB transcript-entry cap can never truncate
content that separate entries carried) and drops superseded cumulative
`ToolUpdate` snapshots. Starts, ends, statuses, errors, titles, `Done` and
`Closed` keep their order and are never merged away. Both bridges run it at the
end of `poll`.

### 6. Instrumentation (opt-in, metadata only)

`src/stream_timing.rs` and the parse hooks in
`components/src/text/text_view.rs` are enabled with:

```sh
KATHAMO_STREAM_TIMINGS=1 cargo run --release
```

Every hook is one atomic load when disabled. Every 5 seconds the process prints
percentiles for:

| Label | Stage |
|---|---|
| `engine.rpi.spawn`, `engine.omp.spawn` | child process creation |
| `engine.*.first_record` | spawn → first RPC record |
| `bridge.*.decode`, `bridge.*.project` | reader-thread parsing/projection |
| `ui.drain` | draining one bridge channel on the UI thread |
| `transcript.enqueue` | serialize + enqueue on the UI thread |
| `transcript.write` | writer-thread disk write |
| `ui.apply` | one event applied to the visible transcript |
| `ui.render_chat`, `ui.render_rows` | chat element construction |
| `markdown.initial_parse`, `markdown.update_parse` | first and later parses |
| `tool.arrival_to_apply` | tool-start RPC → transcript |
| `tool.arrival_to_card` | tool-start RPC → first visible card/group |
| `tool.arrival_to_output` | tool-start RPC → first painted output |

No text, tool arguments or tool output is recorded.

## Measured before/after

These are CPU microbenchmarks from the ignored test
`streaming_hot_path_microbench`, run with the same build:

```sh
cargo test -p kathamo streaming_hot_path_microbench -- --ignored --nocapture
```

They compare the old and new implementations of the stage in-process. They are
not end-to-end GUI timings and do not include the external engine.

| Rank | Stage | Before | After | Change |
|---|---|---|---|---|
| 1 | 2,000 deltas on a ~180 KiB streaming message (split + settled/tail copy) | 300.56 ms | 11.30 ms | ~26.6× less CPU |
| 2 | 10,000 tool-header renders (JSON parse ×3 per frame) | 225.69 ms | 4.65 µs | parse eliminated |
| 3 | 10,000 queued deltas coalesced | 10,000 events | 4 events in 0.27 ms | ~2,500× fewer UI/transcript events |
| 4 | 2,000 history appends, foreground cost | 2.58 ms | 1.27 ms | ~2× less foreground CPU; disk write moved off-thread |

The history row measures only the foreground half now; the writer-thread
`transcript.write` cost is reported separately by the instrumentation.

## Verification

- `cargo check --workspace` passes.
- `cargo clippy --workspace --all-targets` completes; remaining warnings are the
  repository's pre-existing lints.
- `cargo test --workspace` passes: 230 passed, 1 ignored (the benchmark above).
- `deterministic_replay_matches_the_live_transcript` feeds one synthetic RPC
  batch through the live reducer and through `coalesce_events` + the reducer and
  asserts identical roles, text, streaming state and tool output/state.
- `full_and_delta_message_updates_apply_only_the_increment` covers OMP's older
  full-message projection and the delta projection.
- `coalescing_merges_deltas_and_keeps_structural_events`,
  `coalescing_does_not_cross_structural_events_or_the_entry_cap`,
  `streaming_split_is_cached_after_each_delta` and
  `oversized_tool_output_is_previewed_without_losing_the_full_text` cover the
  new invariants.
- Existing GPUI render tests still pass, including the live-reasoning layout
  test.

A live engine probe was attempted without invoking a model (`get_state` only).
rpi 0.21.0 exits with `No API key` before serving that request, so engine-side
spawn/first-byte numbers were not collected in this environment; the
instrumentation is ready for a credentialed run.

A live acceptance pass should still exercise, on both engines: first tool card,
first output paint, incremental stdout/stderr, reasoning expansion, grouping and
completion, cancellation, model switching, scroll lock, copy and transcript
replay/clearing. Run it with `KATHAMO_STREAM_TIMINGS=1` to collect the
arrival→apply/card/output and render/parse percentiles.

## Remaining costs and why they were not changed

- **rpi's bash progress throttle (2 s)** starts at tool execution and can defer
  an incremental update until the window elapses or final capture flushes.
  This is inside `rpi-tools` and intentionally not patched here.
- **OMP's embedded shell / first raw byte** is not observable from Kathamo. The
  bridge measures the first RPC record, not the first byte written by the shell.
  Do not substitute one for the other.
- **Row construction is still O(visible transcript).** Virtualizing with GPUI's
  variable-height `list` changes scroll anchoring, follow-tail and row identity,
  and needs live measurement to validate. The new `ui.render_rows`,
  `ui.render_chat` and `markdown.*` labels are the gate: if those percentiles
  dominate, the list migration is the next step. The current changes already
  remove the per-token rescan and the per-frame tool JSON parsing that made row
  construction expensive.
- **Settled assistant/user text is still cloned into the text view each frame.**
  The text view caches its parsed tree and throttles updates, so this is a copy,
  not a reparse. If `ui.render_rows` stays high after virtualization work, the
  next step is storing immutable row text as a `SharedString` alongside the
  mutable streaming buffer.
- **`tool_group_label` still walks the group's parts per render**, and
  `update_chat_tool` still searches back to the last user message per update.
  Coalescing bounds how often the latter runs. Both are candidates for cached
  row descriptors.
- **The writer queue is unbounded.** Coalescing keeps a burst to a few events
  per 16 ms tick; a bounded queue with an explicit drop/backpressure policy would
  be the next hardening step if disk throughput ever falls behind.
- **Shell launch policy, tool concurrency, early dispatch, model-facing
  truncation and prompt caching remain in rpi/OMP** by design.

## Source map

- `src/conversations.rs`: `WriteJob`, `writer_loop`, `write_line`,
  `Log::append`/`flush`, `clear_all`.
- `src/agent_bridge.rs`: `coalesce_events`, reader-thread decode/project
  timings, `decode_tool_event` arrival hook.
- `src/omp_bridge.rs`: reader-thread projection, approval scan, background
  title read, `poll` coalescing.
- `src/main.rs`: `StreamView`, `refresh_stream_view`, `push_chat_event`,
  `ToolDisplay`, `output_preview_len`, `tool_output_fields`,
  `chat_console_output`, `toggle_chat_tool_show_full`, `poll_agent_events`,
  `render_agent_chat`.
- `src/stream_timing.rs`: opt-in timing and tool-ID correlation.
- `components/src/text/text_view.rs`: `record_parse` hooks for initial and
  update markdown parses.
