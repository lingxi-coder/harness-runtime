# Harness Runtime

The multi-platform Rust runtime extracted from LingXi: agent execution, model host services, sessions, permissions, tools, plugins, platform implementations, and desktop/mobile composition. Existing LingXi namespace defaults and persisted data formats are retained.

The main package is `harness-runtime`. Its `api`, `models`, `desktop`, and `mobile` modules remain available. The default feature is `core`; use `desktop` for desktop composition or `mobile` with optional `uniffi` for native bindings. Hosts inject product build information through Rust configuration; `runtime_build_info()` identifies the runtime separately.

Consumers must pin every shared package to the same Git URL and full commit SHA. The independent `llm-client` dependency remains pinned. Rust 1.94 is selected by `rust-toolchain.toml`.

```sh
cargo check --locked -p harness-runtime --no-default-features
cargo check --locked -p harness-runtime --features desktop
cargo check --locked -p harness-runtime --no-default-features --features mobile,uniffi
cargo test --locked --workspace --all-features --no-fail-fast
./scripts/check-all.sh
```

The repository root is the Cargo workspace; `crates/` preserves component-relative source/resource layout. Vendored sources keep their patches and licenses. Build tools consume source read-only and support separate output/cache directories. No LingXi checkout is required. Product UI, executable entrypoints, native wrappers, and signing remain in LingXi. See `docs/mobile-linux/RUNTIME-SOURCE-CONTRACT.md` for resource integration and `docs/migration/source-manifest.json` for extraction provenance.

See the [validation record](docs/migration/validation.md) for extraction evidence and remaining limitations.
