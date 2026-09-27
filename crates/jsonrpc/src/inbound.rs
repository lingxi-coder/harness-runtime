//! Inbound request dispatch — peers can send us JSON-RPC requests too.
//! `Dispatcher` is a method-name → handler registry. Unknown methods yield
//! a JSON-RPC `MethodNotFound` response automatically.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::messages::{Request, Response, ResponseError, METHOD_NOT_FOUND};

/// Handler for an inbound peer request. Implementors take a `Request`,
/// produce a `Response`.
#[async_trait]
pub trait InboundHandler: Send + Sync + 'static {
    /// Handle one peer request and produce the response that should be
    /// written back over the same connection.
    async fn handle(&self, req: Request) -> Response;
}

/// Type-erased handler stored in the dispatcher map.
pub type BoxedHandler = Arc<dyn InboundHandler>;

/// Method-name registry. Clone-cheap (Arc inside).
#[derive(Clone, Default)]
pub struct Dispatcher {
    inner: Arc<RwLock<HashMap<String, BoxedHandler>>>,
}

impl Dispatcher {
    /// Construct an empty dispatcher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `handler` to receive requests whose `method` equals `name`.
    /// Replaces any existing handler for that method.
    pub async fn register(&self, name: impl Into<String>, handler: BoxedHandler) {
        self.inner.write().await.insert(name.into(), handler);
    }

    /// Dispatch one peer request. If no handler is registered, returns a
    /// `MethodNotFound` response so the writer side can ship it.
    pub async fn dispatch(&self, req: Request) -> Response {
        let handler = {
            let map = self.inner.read().await;
            map.get(&req.method).cloned()
        };
        match handler {
            Some(h) => h.handle(req).await,
            None => Response::error(
                Some(req.id),
                ResponseError {
                    code: METHOD_NOT_FOUND,
                    message: format!("method not found: {}", req.method),
                    data: None,
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{Id, JSONRPC_VERSION};
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl InboundHandler for Echo {
        async fn handle(&self, req: Request) -> Response {
            Response::success(req.id, req.params.unwrap_or(json!(null)))
        }
    }

    #[tokio::test]
    async fn dispatch_returns_method_not_found_for_unregistered_method() {
        let d = Dispatcher::new();
        let req = Request::new("missing", None, Id::Number(1));
        let resp = d.dispatch(req).await;
        assert_eq!(resp.jsonrpc, JSONRPC_VERSION);
        let err = resp.error.expect("error response");
        assert_eq!(err.code, METHOD_NOT_FOUND);
        assert!(err.message.contains("missing"));
        assert_eq!(resp.id, Some(Id::Number(1)));
        assert!(resp.result.is_none());
    }

    #[tokio::test]
    async fn dispatch_calls_registered_handler() {
        let d = Dispatcher::new();
        d.register("echo", Arc::new(Echo)).await;
        let req = Request::new("echo", Some(json!({"x": 1})), Id::Number(42));
        let resp = d.dispatch(req).await;
        assert_eq!(resp.id, Some(Id::Number(42)));
        assert_eq!(resp.result, Some(json!({"x": 1})));
        assert!(resp.error.is_none());
    }

    struct Two;
    #[async_trait]
    impl InboundHandler for Two {
        async fn handle(&self, req: Request) -> Response {
            Response::success(req.id, json!(2))
        }
    }

    #[tokio::test]
    async fn register_overwrites_existing_handler() {
        let d = Dispatcher::new();
        d.register("m", Arc::new(Echo)).await;
        d.register("m", Arc::new(Two)).await;
        let resp = d.dispatch(Request::new("m", None, Id::Number(1))).await;
        assert_eq!(resp.result, Some(json!(2)));
    }
}
