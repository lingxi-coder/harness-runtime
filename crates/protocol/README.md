# lingxi-protocol

Shared protocol DTOs for the LingXi Core engine. Owns boundary types — IDs (`AgentId`, `SessionId`, `MessageId`, `ToolUseId`, `RequestId`, ...), message DTOs (`ConversationMessage`, `ContentBlock`), transport DTOs (`HttpRequest`, `HttpResponse`, `SseEvent`), platform capability flags (`PlatformCapabilities`), and the `Effect`/`EffectResult`/`EffectError` triple that crosses the reducer ↔ EffectHandler boundary. No I/O, no OS deps, no `tokio`. Both `lingxi-core` and `lingxi-traits` depend on this crate to avoid a cyclic dependency.

See `docs/superpowers/specs/2026-05-22-lingxi-core-rust-engine-design.md` §3 (D16 Shared protocol boundary).
