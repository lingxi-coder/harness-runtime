# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A multi-platform Rust runtime extracted from LingXi (agent execution, model host layer, sessions, permissions, tools, plugins, platform adapters, desktop/mobile composition). Product UI, CLI/TUI entrypoints, native wrappers and signing stay in LingXi — this repo has no executable product. LingXi namespace defaults and persisted data formats are deliberately retained; don't rename them.

## Commands

Rust 1.94 is pinned by `rust-toolchain.toml`. Always pass `--locked`.

```sh
git submodule update --init --recursive           # deps/llm-client is required to build

# Feature-combination checks (each must pass independently)
cargo check --locked -p harness-runtime --no-default-features
cargo check --locked -p harness-runtime --features desktop
cargo check --locked -p harness-runtime --no-default-features --features mobile,uniffi

cargo test --locked --workspace --all-features --no-fail-fast
cargo test --locked -p <crate> --test <test_name>  # single integration test
cargo test --locked -p test-harness --test 'parity_*'   # parity drivers
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings

./scripts/check-all.sh                             # every repo gate (see below)
cargo test --manifest-path deps/llm-client/Cargo.toml --locked   # SDK's own tests
```

## Architecture

- **Composition root.** `crates/runtime` (package `harness-runtime`) is the only place that makes shipping choices; it re-exports `api`, `models`, `desktop`, `mobile` modules. Features: `engine` (default: orchestrator, agent, session, permission, hooks, mcp, compaction, tools APIs…), `desktop` (adds branding, cron, memory, migrations, provider-config…), `mobile`, plus `uniffi` for native bindings. Hosts inject product build info through Rust config; `runtime_build_info()` reports the runtime's own identity.
- **Dependency direction is enforced** by `scripts/check-deps.sh` (reads `cargo metadata`): library crates flow strictly "downhill". Tools (`crates/tools/*`) may not depend on other tools or platforms; platforms (`crates/platforms/*`) may not depend on tools; engine crates at `crates/<x>` may not depend on tools/platforms. Fix the layering rather than adding an exemption.
- **Model networking goes only through `llm-client`.** `scripts/check_llm_boundary.py` rejects provider/model endpoint construction anywhere else (general HTTP — web fetch, OAuth, MCP, telemetry — stays host-owned). The SDK lives in the `deps/llm-client` submodule, wired in via a root `[patch]`; the canonical Git URL + full rev is in root `[workspace.dependencies]`. The patch does not propagate downstream: commit and push SDK changes in the submodule repo first, then bump the submodule and the pinned rev here. See `docs/llm-client-upgrade.md`.
- **Branding.** Brand-specific names/paths live in `crates/branding`. `scripts/check-brand-leaks.sh` compares a violation set against `scripts/brand_leak_baseline.txt` (both additions and disappearances fail); use `python3 scripts/check_brand_leaks.py --list` to see violations.
- **Unsafe code:** workspace lint is `deny`; every crate adds `#![forbid(unsafe_code)]` in `lib.rs`, except the one `platform-posix` module that needs `setsid()`. New crates must follow this.
- Workspace `default-members` excludes test fixtures (e.g. `mock_stdio_mcp`); keep new fixtures out of it.
- `third_party/` holds vendored, patched sources with their own licenses; `deps/llm-client` and those are excluded from the workspace.
- `skills/` are plugin skills; `check-skill-frontmatter.sh` caps `description` at 180 **display columns** (CJK counts double — `len()` is wrong).

## Gates

`scripts/check-all.sh` discovers every executable `scripts/check-*.sh` and `scripts/*-gate.sh` and runs them; `scripts/test_gate_triggers.py` cross-checks that they actually ran. A new gate only needs that naming and the executable bit. Gate scripts must be invoked as `./scripts/<name>.sh` from the repo root. Gates are designed to fail closed — keep that property (no process-substitution loops that silently iterate zero times).

## References

- `docs/mobile-linux/RUNTIME-SOURCE-CONTRACT.md` — resource integration contract for downstream mobile/Linux hosts.
- `docs/migration/source-manifest.json`, `docs/migration/validation.md` — extraction provenance and known limitations.
- Downstream consumers must pin every shared package to the same Git URL and full commit SHA.
