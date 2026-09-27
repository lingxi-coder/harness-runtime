# platform-api

Platform abstraction traits for LingXi. Library crates depend on these traits; platform crates (`platforms/posix`, `platforms/windows`, etc.) implement them. Provides:
- `HttpTransport` — provider-neutral HTTP + SSE abstraction.
- `Clock` — wall-clock time abstraction.
- `RuntimeSpawner` — background task spawning (the engine never calls `tokio::spawn` directly — see D17).
- `EffectHandler` — the I/O boundary for the reducer's emitted `Effect` values.
- `FileSystem` — file I/O abstraction (signatures only; impls in platform crates).

See `docs/superpowers/specs/2026-05-22-lingxi-core-rust-engine-design.md` §4 (Trait System) and D17 (Runtime boundary).
