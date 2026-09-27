//! `lingxi-jsonrpc` — protocol-agnostic JSON-RPC 2.0 framing and routing.
//!
//! Used by `lingxi-mcp`, `lingxi-bridge`, and `lingxi-lsp`. See plan
//! `docs/superpowers/plans/2026-05-23-m2-02a-jsonrpc.md`.

#![forbid(unsafe_code)]

pub mod broker;
pub mod codec;
pub mod connection;
pub mod inbound;
pub mod messages;
pub mod router;

pub use broker::{spawn as spawn_broker, BrokerError, BrokerHandle, DEFAULT_NOTIFICATION_CAPACITY};
pub use codec::{CodecError, LineCodec, LspCodec};
pub use connection::{Connection, ConnectionBuilder, ConnectionError, ConnectionMode, Mode};
pub use inbound::{BoxedHandler, Dispatcher, InboundHandler};
pub use messages::{
    Id, Message, Notification, Request, Response, ResponseError, INTERNAL_ERROR, INVALID_PARAMS,
    INVALID_REQUEST, JSONRPC_VERSION, METHOD_NOT_FOUND, PARSE_ERROR,
};
pub use router::{OutboundMessage, Router, RouterError, StartedCall, DEFAULT_TIMEOUT};

// JS-protocol-name aliases for ergonomic dual naming. Consumer crates may
// `use jsonrpc::JsonRpcError;` or `use jsonrpc::RequestId;` and
// get the canonical Rust types without going through the long path.
pub use messages::Id as RequestId;
pub use messages::ResponseError as JsonRpcError;

/// Crate version constant — kept in sync with `Cargo.toml`.
pub const VERSION: &str = "0.1.0";
