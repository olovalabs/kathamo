# Embedded rpi agent

Kathamo now runs rpi conversations in-process by default. The `rpi`
executable, `--mode rpc`, stdin/stdout pipes and the JSONL RPC transport are no
longer used for the rpi backend. OMP remains a separate subprocess backend.

## Upstream source and ownership

| Item | Value |
|---|---|
| Repository | https://github.com/bigfish1913/pi-rust |
| Tag | `v0.21.0` |
| Resolved commit | `02d4a961cf9fd31285173f61bf06cf0e51435c82` |
| Location | `agent/` |
| Import commit | `161ec18` (`Squashed 'agent/' content from commit 02d4a96`) |

The upstream library crates, CLI sources, license and in-crate tests are in
`agent/`. The initial import contained the complete tracked source; after that,
the upstream website, docs, scripts, packaging, deploy/demo assets, CI metadata
and other standalone examples were removed so the vendor tree contains only what
Kathamo builds. `examples/plugin-stub` remains because upstream's
resource-selection tests build it as a fixture. It remains its own Cargo
workspace. `Cargo.toml` at the Kathamo root path-depends on:

```toml
rpi-embedded = { path = "agent/crates/rpi-embedded" }
```

`git subtree add` refused because the working tree already had the streaming
changes. The equivalent subtree import was performed with:

```sh
git fetch pi-rust tag v0.21.0
git read-tree --prefix=agent/ -u 02d4a961cf9fd31285173f61bf06cf0e51435c82
git commit   # standard git-subtree-dir / git-subtree-split trailers
```

The commit message records `git-subtree-dir: agent` and
`git-subtree-split: 02d4a96…`, so `git subtree` tooling can still pull/push the
subtree.

## The embedded entry point

`agent/crates/rpi-embedded` is the host-facing library. It is built from the
upstream library crates (`rpi-cli`, `rpi-harness`, `rpi-tools`, `rpi-runtime`,
`rpi-ai`, `rpi-agent`, `rpi-extensions`) and exposes:

- `EmbeddedSession` — one conversation on a dedicated multi-thread Tokio
  runtime.
- `SessionEvent` — typed text, reasoning, tool start/update/end, status, error,
  completion and closed events.
- `catalog()` / `providers()` — in-process model and credential discovery.
- `Login` — in-process API-key and extension-owned OAuth login.

`EmbeddedSession` reuses `rpi_cli::session::build`, which is upstream's own
startup path: `OsExecutionEnv` rooted at the session cwd, the built-in
`read`/`bash`/`edit`/`write`/`docs` tools, provider/model/credential resolution,
the coding system prompt, durable JSONL session storage and `AgentHarness`.
There is no reimplementation of the agent loop or tools.

### Lifecycle

1. `EmbeddedSession::spawn` returns immediately on the UI thread and starts a
   worker thread with `tokio::runtime::Builder::new_multi_thread`.
2. The worker resolves the model (`rpi_cli::provider::resolve_for_cwd`) off the
   async workers, then calls `rpi_cli::session::build`.
3. The durable session id is published from `AgentHarness::session_id` and read
   by Kathamo's existing `poll_agent_events` loop.
4. A forwarding task drains the harness `broadcast` bus, maps `rpi_agent::AgentEvent`
   to `SessionEvent`, and sends into Kathamo's polling channel.
5. Prompts run as Tokio tasks. `Stop` sends `request_stop_round` and awaits
   `abort_reclaiming(true)`, which cancels active tools. Dropping the session
   sends `Shutdown` and gives cancellation two seconds before detaching the
   reap thread.
6. A stale session id is retried as a fresh durable session, matching the old
   bridge's resume fallback.

Events use the same `agent_bridge::AgentEvent` contract as before through a thin
adapter. The existing transcript reducer, coalescing, tool grouping, markdown,
scroll and copy paths are unchanged.

### Session isolation

Embedded sessions set `no_extensions` in `rpi_cli::session::build`. rpi's
extension layer publishes a process-global session id and provider hooks read it
per turn; running several panes in one process with plugins enabled would let one
conversation observe another's identity. The embedded core therefore uses the
built-in tool set and provider runtimes. Login still loads extensions when it
needs an OAuth provider and keeps them scoped to that login.

## In-process model discovery and login

`src/agent_auth.rs` no longer runs `rpi --offline --list-models` or
`rpi auth login`:

- Model catalog: `rpi_embedded::catalog()` reads the same built-in catalog and
  `models.json` through `rpi_cli::provider::catalog_all`.
- Provider readiness: `rpi_embedded::providers()` resolves the same
  `models.json` + `auth.json` + environment sources through
  `RuntimeConfig::auth_states`, so a provider is reported ready only when it
  actually is.
- rpi-only providers: `agent_auth::Login` starts `rpi_embedded::Login`. API keys
  are persisted with `rpi_cli::config::upsert_credential`; OAuth providers are
  driven through their extension's `begin`/`exchange` actions and saved with
  `upsert_oauth_token`.
- OMP providers keep the existing `omp login` PTY. The OMP backend itself is
  unchanged.

## Baseline and measurements

The old subprocess path is preserved as an opt-in comparator:

```sh
KATHAMO_RPI_BACKEND=rpc ./target/release/kathamo
```

The default (unset or any other value) is the embedded backend. Both backends
feed the same UI and timing labels, so a credentialed machine can compare them
without rebuilding:

```sh
KATHAMO_STREAM_TIMINGS=1 ./target/release/kathamo
KATHAMO_DIAGNOSTICS=1 ./target/release/kathamo
KATHAMO_RPI_BACKEND=rpc KATHAMO_STREAM_TIMINGS=1 ./target/release/kathamo
```

Relevant labels:

| Comparison | Embedded | RPC baseline |
|---|---|---|
| Startup | `engine.embedded.spawn`, `engine.embedded.ready` | `engine.rpc.spawn`, `engine.rpc.first_record` |
| Tool event → apply | `tool.arrival_to_apply` | `tool.arrival_to_apply` |
| Tool event → first card | `tool.arrival_to_card` | `tool.arrival_to_card` |
| Tool event → first output paint | `tool.arrival_to_output` | `tool.arrival_to_output` |
| UI apply / render | `ui.apply`, `ui.render_chat`, `ui.render_rows` | same |
| Markdown parse | `markdown.initial_parse`, `markdown.update_parse` | same |
| Frame time | `KATHAMO_DIAGNOSTICS` | same |

The embedded forwarder installs `set_tool_arrival_hook`, so the tool timestamp is
taken when the harness event reaches Kathamo's forwarder, not when the UI later
polls it. The old RPC bridge takes its timestamp in the stdout reader.

The 2-second bash checkpoint throttle is unchanged by default. It can be varied
as a separate experiment without rebuilding:

```sh
RPI_BASH_UPDATE_THROTTLE_MS=100 KATHAMO_STREAM_TIMINGS=1 ./target/release/kathamo
```

This isolates the shell-throttle contribution from the embedding change. The
first raw shell byte is still only observable inside `rpi-tools`; Kathamo does
not substitute the first RPC update for it.

A live credentialed model run was not performed in this environment. The RPC
baseline, the timing labels and the throttle knob are in place for that run.

## Verification performed

Kathamo:

- `cargo fmt --check` — pass.
- `cargo check --workspace` — pass.
- `cargo clippy --workspace --all-targets` — pass; remaining warnings are the
  repository's pre-existing lints.
- `cargo test --workspace` — 230 passed, 1 ignored.
- `cargo build --release` — pass.
- `PATH=/usr/bin:/bin ./target/release/kathamo --version` — prints
  `kathamo 0.0.1`, so the release binary runs without `rpi` on `PATH`.

Imported workspace:

- `cargo check --workspace` in `agent/` — pass.
- `cargo clippy --workspace --all-targets` in `agent/` — pass. The upstream
  `clippy::not_unsafe_ptr_arg_deref` denial on the `rpi-extensions` FFI
  trampoline and the `export_plugin!` ABI shim was annotated with an `#[allow]`
  and a comment; no runtime behavior changed.
- `cargo test -p rpi-embedded -p rpi-runtime -p rpi-agent -p rpi-tools -p rpi-harness`
  — pass.
- `cargo test -p rpi-cli --lib` — 1,014 passed, 3 ignored.
- `cargo test --workspace` in `agent/` — pass (CLI + TUI + extensions + examples).
- The compiled `rpi_embedded` test binary passes all six tests with
  `PATH=/usr/bin:/bin`, including:
  - a full faux-provider harness run (text deltas + completion),
  - a missing-model worker failure (`Error` → `Done` → `Closed`),
  - event mapping for text, tools, retries and completion.

## Remaining costs and limitations

- **Extensions are not loaded for embedded sessions.** This is deliberate:
  upstream extension identity is process-global. Built-in tools and providers,
  `models.json`, `auth.json`, environment credentials and durable sessions are
  fully reused. Login still loads extensions for OAuth.
- **Extension-provided models are not in the embedded catalog.** The catalog is
  the built-in catalog plus `models.json`, matching a session built with
  `no_extensions`.
- **OAuth token refresh at session start is not wired.** Initial OAuth login is
  in-process and persisted with upstream storage. Expired stored tokens must be
  re-authenticated; extension-based refresh can be added once extension loading
  is made session-scoped.
- **Each session owns a Tokio runtime.** This isolates cancellation, tools and
  session storage per conversation; the cost is one small runtime and worker
  thread set per active conversation.
- **`rpi-cli` is compiled into Kathamo.** This is the price of reusing upstream
  startup; it increases compile time and binary size.
- **The shell checkpoint throttle defaults to 2 s.** It can be measured
  separately with `RPI_BASH_UPDATE_THROTTLE_MS`; it was not lowered by default.
- **The first raw shell byte is not exposed by the harness event bus.** The
  measured output milestone is the first tool-update event/paint.

## Source map

- `agent/` — upstream pi-rust v0.21.0 subtree, own workspace.
- `agent/crates/rpi-embedded/src/lib.rs` — `EmbeddedSession`, `SessionEvent`,
  `catalog`, `providers`, `Login`.
- `agent/crates/rpi-cli/src/session.rs` — upstream `build` path; the embedded
  event buffer is 8192.
- `agent/crates/rpi-tools/src/tools/bash.rs` — separate
  `RPI_BASH_UPDATE_THROTTLE_MS` measurement knob.
- `src/agent_bridge.rs` — embedded/RPC backend selection, event adapter,
  coalescing, startup/tool timing hooks.
- `src/agent_bridge_rpc.rs` — preserved subprocess baseline
  (`KATHAMO_RPI_BACKEND=rpc`).
- `src/agent_auth.rs` — in-process catalog, provider readiness and login.
- `src/main.rs` — rpi-only providers start the in-process login; the tool arrival
  hook is installed at startup.
- `Cargo.toml` — path dependency on `agent/crates/rpi-embedded`.
