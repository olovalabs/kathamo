#!/usr/bin/env python3
"""Compare internal pipeline costs offline, without changing the checkout."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-ref", default="87c29e9")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--target-dir", type=Path)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    repo = Path(__file__).resolve().parent.parent
    baseline = subprocess.check_output(
        ["git", "rev-parse", args.baseline_ref], cwd=repo, text=True, encoding="utf-8").strip()
    target = (args.target_dir or repo / "target" / "pipeline-bench").resolve()
    with tempfile.TemporaryDirectory(prefix="rpi-pipeline-bench-") as directory:
        fixture = Path(directory)
        for name, source in (
            ("baseline_state.rs", "crates/rpi-harness/src/session/state.rs"),
            ("baseline_context.rs", "crates/rpi-harness/src/session/context.rs"),
        ):
            data = subprocess.check_output(["git", "show", f"{baseline}:{source}"], cwd=repo)
            (fixture / name).write_bytes(data)
        # `state.rs` declares `mod snapshot;`. Because it is staged as a
        # sibling file (`baseline_state.rs`), Rust resolves that child module
        # under `baseline_state/snapshot.rs`, not beside the fixture root.
        # Copy the baseline child module as well so older revisions remain
        # runnable by this offline benchmark.
        baseline_snapshot = fixture / "baseline_state" / "snapshot.rs"
        baseline_snapshot.parent.mkdir()
        baseline_snapshot.write_bytes(
            subprocess.check_output(
                ["git", "show", f"{baseline}:crates/rpi-harness/src/session/state/snapshot.rs"],
                cwd=repo,
            )
        )
        sources = {
            "rpc_events.rs": "crates/rpi-cli/src/rpc/events.rs",
            "protocol.rs": "crates/rpi-cli/src/remote/protocol.rs",
            "prepared_request.rs": "crates/rpi-ai/src/providers/request.rs",
        }
        for name, source in sources.items():
            (fixture / name).write_bytes((repo / source).read_bytes())
        # Read-only instrumentation inside the copied module. Production code
        # stays untouched, and no request is sent by the HTTP encoding case.
        with (fixture / "prepared_request.rs").open("a", encoding="utf-8") as stream:
            stream.write("\npub fn clone_attempt(p: &PreparedJsonRequest) -> reqwest::Request "
                         "{ p.0.request.try_clone().unwrap() }\n")
        source = repo / "scripts/bench-pipeline/main.rs"
        (fixture / "main.rs").write_bytes(source.read_bytes())
        manifest = """[package]
name = "rpi-pipeline-bench"
version = "0.0.0"
edition = "2021"
[dependencies]
"""
        for package in ("rpi-harness", "rpi-agent", "rpi-tools"):
            manifest += f'{package} = {{ path = "{(repo / "crates" / package).as_posix()}" }}\n'
        manifest += f'rpi-ai = {{ path = "{(repo / "crates/rpi-ai").as_posix()}", features = ["providers"] }}\n'
        manifest += """serde = { version = "1", features = ["derive", "rc"] }
serde_json = { version = "1", features = ["preserve_order", "raw_value"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
tempfile = "3"
[profile.release]
lto = "thin"
codegen-units = 16
[[bin]]
name = "pipeline-bench"
path = "main.rs"
"""
        (fixture / "Cargo.toml").write_text(manifest, encoding="utf-8")
        command = ["cargo", "build", "--release", "--manifest-path", str(fixture / "Cargo.toml"),
                   "--target-dir", str(target)]
        if args.offline:
            command.append("--offline")
        subprocess.run(command, cwd=repo, stdout=subprocess.DEVNULL, check=True)
        executable = target / "release" / ("pipeline-bench.exe" if os.name == "nt" else "pipeline-bench")
        hashed = [*sources.values(), "scripts/bench-pipeline/main.rs",
                  "crates/rpi-harness/src/session/state.rs",
                  "crates/rpi-harness/src/session/state/snapshot.rs",
                  "crates/rpi-harness/src/session/context.rs",
                  "crates/rpi-harness/src/frame_progress.rs",
                  "crates/rpi-harness/src/session/session.rs",
                  "crates/rpi-harness/src/agent_harness.rs",
                  "crates/rpi-harness/src/session/jsonl/storage.rs",
                  "crates/rpi-tools/src/os_env.rs", "crates/rpi-tools/src/os_env/append.rs"]
        print(json.dumps({"environment": platform.platform(), "cpu": platform.processor(),
                          "rustc": subprocess.check_output(["rustc", "-Vv"], text=True, encoding="utf-8"),
                          "baseline_ref": baseline,
                          "binary_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
                          "sources_sha256": {name: hashlib.sha256((repo / name).read_bytes()).hexdigest()
                                             for name in hashed},
                          "notes": "Offline microbenchmarks; not live model, network or UI latency. "
                          "Context/state use baseline source. HTTP compares four encodings with one prepared request; "
                          "RPC compares legacy projection with encoded updates; progress compares JSON records "
                          "with volatile typed frames; file compares the per-call API and optional append writer, with platform-specific handle reuse."}), flush=True)
        for case in ("context", "state", "http", "rpc", "progress", "file"):
            for run in range(args.repeats):
                variants = ("before", "after") if run % 2 == 0 else ("after", "before")
                for variant in variants:
                    result = subprocess.run([str(executable), case, variant], text=True, encoding="utf-8",
                                            capture_output=True, check=True, timeout=60)
                    print(json.dumps(dict(run=run, **json.loads(result.stdout))), flush=True)


if __name__ == "__main__":
    main()
