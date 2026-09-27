//! LSP transport implementation wiring.
//!
//! M1.18 stub. The real implementation lives in `platforms/posix-minimal`
//! (Plan 16) where the JSON-RPC framing, stdio plumbing, and language
//! server handshakes are wired up. The engine only ever holds an
//! `Arc<dyn platform_api::LspTransport>` here.
