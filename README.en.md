# Harness Runtime

The multi-platform Rust runtime extracted from LingXi: agent execution, model host services, sessions, permissions, tools, plugins, platform implementations, and desktop/mobile composition. Existing LingXi namespace defaults and persisted data formats are retained.

The main package is `harness-runtime`, located in `crates/runtime`. Its `api`, `models`, `desktop`, and `mobile` modules remain available. The default feature is `core`; use `desktop` for desktop composition or `mobile` with optional `uniffi` for native bindings. Hosts inject product build information through Rust configuration; `runtime_build_info()` identifies the runtime separately.

Consumers must pin every shared package to the same Git URL and full commit SHA. The independent `llm-client` dependency remains pinned; development in this repository uses the `deps/llm-client` Git submodule through a root-workspace Cargo patch. Rust 1.94 is selected by `rust-toolchain.toml`.

The model layer exposes hosted web search/fetch, remote Skills, audio, files, and realtime sessions through the shared 0.3 client. See the [integration and migration guide](docs/llm-client-upgrade.md) for service boundaries, streaming uploads, and joint development.

Clone with the SDK source:

```sh
git clone --recurse-submodules https://github.com/lingxi-coder/harness-runtime.git
cd harness-runtime
```

For an existing checkout, run `git submodule update --init --recursive`. Changes in
`deps/llm-client` immediately participate in Cargo builds and tests from the repository
root. Run the SDK's own tests separately with
`cargo test --manifest-path deps/llm-client/Cargo.toml --locked`.
Commit and push SDK changes in its repository before updating the parent repository's
submodule revision and pinned dependency in root `[workspace.dependencies]`. The root
Cargo patch does not propagate to downstream projects. See the [joint development workflow](docs/llm-client-upgrade.md#子模块联合开发),
including the publication order for SDK changes.

Validate the runtime with:

```sh
cargo check --locked -p harness-runtime --no-default-features
cargo check --locked -p harness-runtime --features desktop
cargo check --locked -p harness-runtime --no-default-features --features mobile,uniffi
cargo test --locked --workspace --all-features --no-fail-fast
./scripts/check-all.sh
```

The repository root is the Cargo workspace; `crates/` preserves component-relative source/resource layout. Vendored sources keep their patches and licenses. Build tools consume source read-only and support separate output/cache directories. No LingXi checkout is required. Product UI, executable entrypoints, native wrappers, and signing remain in LingXi. See `docs/mobile-linux/RUNTIME-SOURCE-CONTRACT.md` for resource integration and `docs/migration/source-manifest.json` for extraction provenance.

See the [validation record](docs/migration/validation.md) for extraction evidence and remaining limitations.
