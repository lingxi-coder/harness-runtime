# Harness Runtime

A multi-platform Rust runtime extracted from LingXi. It provides agent execution, model host services, sessions and persistence, permissions, tools, plugins, and desktop/mobile composition. This repository supplies embeddable libraries. Product UI, CLI/TUI entrypoints, native host wrappers, and signing remain in LingXi.

The main Cargo package is `harness-runtime` in [`crates/runtime`](crates/runtime); its Rust import is `harness_runtime`. Existing LingXi namespace defaults and persisted data formats are retained.

[简体中文](README.md)

## Quick start

The repository pins Rust 1.94. Clone its `llm-client` submodule on the first checkout:

```sh
git clone --recurse-submodules https://github.com/lingxi-coder/harness-runtime.git
cd harness-runtime
cargo check --locked -p harness-runtime
```

For an existing checkout or after switching commits, run `git submodule update --init --recursive`. Run Cargo from the repository root: its `[patch]` redirects the pinned `lingxi-llm-client` Git dependency to the local `deps/llm-client` source.

## Integrate with a host

Pin a **full 40-character commit SHA** in the host's `Cargo.toml`. For a desktop host:

```toml
[dependencies]
harness-runtime = { git = "https://github.com/lingxi-coder/harness-runtime.git", rev = "<full 40-character commit SHA>", default-features = false, features = ["desktop"] }
```

Replace the placeholder with a published commit. A mobile Rust host uses `features = ["mobile"]`; foreign-language bindings use `features = ["uniffi"]`, which includes `mobile`. If the host depends directly on other packages in this repository, pin **every shared package to the same Git URL and commit**. Otherwise, Cargo can resolve nominally identical types to different package identities. The root `[patch]` in this repository does not propagate to downstream workspaces.

| Feature | Purpose |
| --- | --- |
| `core` (default) | Shared agent, session, orchestration, and permission components; exposes `Harness`, `HarnessBuilder`, and `SessionHandle`. |
| `desktop` | Desktop composition, including `core`; `harness_runtime::desktop` exposes `build` and `build_harness`. |
| `mobile` | Shared iOS/Android Rust composition, including `core`; `harness_runtime::mobile` exposes `build_mobile`. |
| `uniffi` | Native mobile bindings; enables `mobile`. |
| `android-computer-use` | Android device interaction; enables `mobile`. |
| `realtime-websocket` | Enables WebSocket realtime support in the model runtime. |

`HarnessBuilder` accepts host-assembled `SessionService` and `LifecycleService` implementations; it does not start processes or network requests. Desktop hosts can use `desktop::build_harness` to inject an output stream and permission gate. Mobile hosts inject their platform capabilities through `mobile::build_mobile`. The host must stop admitting work and wait for active turns before calling `Harness::shutdown()`. If `ShutdownReport.complete` is `false`, handle its errors and retry sequentially. The host injects product build information; `desktop::runtime_build_info()` and `mobile::runtime_build_info()` report the runtime's own identity separately.

Once the host has assembled a `Harness`, run a turn through the shared session interface:

```rust
use harness_runtime::api::{CancellationToken, HandleError, RunInput, TurnOutcome};
use harness_runtime::Harness;

async fn run_turn(harness: &Harness, prompt: String) -> Result<TurnOutcome, HandleError> {
    harness.session().run(
        RunInput { prompt, images: Vec::new() },
        CancellationToken::new(),
    ).await
}
```

Model requests use `harness_runtime::models::llm` and the independent `llm-client`. See the [client integration guide](docs/llm-client-upgrade.md) for hosted Web Search/Web Fetch, remote Skills, audio, files, realtime sessions, examples, and ownership boundaries.

## Repository layout

| Path | Contents |
| --- | --- |
| [`crates/runtime`](crates/runtime) | Public composition entrypoint and desktop/mobile assembly. |
| [`crates/core`](crates/core), [`crates/agent`](crates/agent), [`crates/orchestrator`](crates/orchestrator) | State and agent execution components. |
| [`crates/client`](crates/client), [`crates/protocol`](crates/protocol) | Client protocol, presentation, and runtime adapters. |
| [`crates/tools`](crates/tools), [`crates/platforms`](crates/platforms) | Tool implementations and platform capabilities. |
| [`deps/llm-client`](deps/llm-client), [`third_party`](third_party) | Independent SDK submodule and vendored sources with their own patches and licenses. |
| [`scripts`](scripts), [`docs`](docs) | Build/boundary checks and design/migration documents. |

`scripts/check-deps.sh` enforces dependency direction. `llm-client` owns model provider communication; the host retains credentials, sessions, permissions, tool execution, and persistence. The [runtime source contract](docs/mobile-linux/RUNTIME-SOURCE-CONTRACT.md) covers mobile Linux resources and host integration.

## Development and validation

```sh
cargo check --locked -p harness-runtime --no-default-features
cargo check --locked -p harness-runtime --features desktop
cargo check --locked -p harness-runtime --no-default-features --features mobile,uniffi
cargo test --locked --workspace --all-features --no-fail-fast
./scripts/check-all.sh
```

These commands cover minimal, desktop, mobile-binding, workspace-test, and repository-gate configurations. Native devices, complete product packaging, and signing require separate host validation. See the [validation record](docs/migration/validation.md) for completed migration checks and known limitations.

Changes in `deps/llm-client` participate immediately in root-workspace builds. Run the SDK's own tests separately:

```sh
cargo test --manifest-path deps/llm-client/Cargo.toml --locked
```

For a joint release, validate, commit, and push the SDK first; then update this repository's submodule commit, pinned revision in root `[workspace.dependencies]`, and `Cargo.lock`. See the [joint development workflow](docs/llm-client-upgrade.md#子模块联合开发). The [source manifest](docs/migration/source-manifest.json) records extraction provenance.
