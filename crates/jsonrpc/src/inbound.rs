//! Inbound request dispatch — peers can send us JSON-RPC requests too.
//! `Dispatcher` is a method-name → handler registry. Unknown methods yield
//! a JSON-RPC `MethodNotFound` response automatically.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::messages::{JsonRpcString, Request, Response, ResponseError, METHOD_NOT_FOUND};

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
    method_not_found_message: Option<JsonRpcString>,
}

impl Dispatcher {
    /// Construct an empty dispatcher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a dispatcher whose unknown-method error uses a fixed message.
    /// Protocol adapters can select this at connection construction while
    /// generic JSON-RPC dispatch keeps its method-bearing default message.
    #[must_use]
    pub fn with_method_not_found_message(message: impl Into<JsonRpcString>) -> Self {
        Self {
            inner: Arc::default(),
            method_not_found_message: Some(message.into()),
        }
    }

    /// Register `handler` to receive requests whose `method` equals `name`.
    /// Replaces any existing handler for that method.
    pub async fn register(&self, name: impl Into<String>, handler: BoxedHandler) {
        self.inner.write().await.insert(name.into(), handler);
    }

    /// Clone the registered local handler without retaining the registry lock.
    pub async fn get_registered(&self, method: &str) -> Option<BoxedHandler> {
        self.inner.read().await.get(method).cloned()
    }

    /// Dispatch one peer request. If no handler is registered, returns a
    /// `MethodNotFound` response so the writer side can ship it.
    pub async fn dispatch(&self, req: Request) -> Response {
        let handler = {
            let map = self.inner.read().await;
            String::from_utf16(req.method.code_units())
                .ok()
                .and_then(|method| map.get(&method).cloned())
        };
        match handler {
            Some(h) => h.handle(req).await,
            None => Response::error(
                Some(req.id),
                ResponseError {
                    code: METHOD_NOT_FOUND,
                    message: self.method_not_found_message.clone().unwrap_or_else(|| {
                        let mut units = "method not found: ".encode_utf16().collect::<Vec<_>>();
                        units.extend_from_slice(req.method.code_units());
                        JsonRpcString::from_code_units(units)
                    }),
                    data: None,
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{Id, JsonPayload, JSONRPC_VERSION};
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl InboundHandler for Echo {
        async fn handle(&self, req: Request) -> Response {
            Response::success(
                req.id,
                req.params
                    .unwrap_or_else(|| JsonPayload::parse("null").unwrap()),
            )
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
        assert!(err.message.display().contains("missing"));
        assert_eq!(resp.id, Some(Id::Number(1)));
        assert!(resp.result.is_none());
    }

    #[tokio::test]
    async fn dispatch_calls_registered_handler() {
        let d = Dispatcher::new();
        d.register("echo", Arc::new(Echo)).await;
        let req = Request::new(
            "echo",
            Some(JsonPayload::from_serializable(&json!({"x": 1})).unwrap()),
            Id::Number(42),
        );
        let resp = d.dispatch(req).await;
        assert_eq!(resp.id, Some(Id::Number(42)));
        assert_eq!(resp.result.unwrap().to_value().unwrap(), json!({"x": 1}));
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn unknown_method_error_preserves_exact_utf16_units_by_default() {
        let dispatcher = Dispatcher::new();
        let request = Request::new(
            JsonRpcString::from_code_units(vec![b'm' as u16, 0xd800]),
            None,
            Id::Number(7),
        );

        let response = dispatcher.dispatch(request).await;
        let message = response.error.expect("method-not-found error").message;
        let mut expected = "method not found: m".encode_utf16().collect::<Vec<_>>();
        expected.push(0xd800);
        assert_eq!(message.code_units(), expected.as_slice());
    }

    #[tokio::test]
    async fn fixed_method_not_found_message_does_not_interpolate_method() {
        let dispatcher = Dispatcher::with_method_not_found_message("Method not found");
        let request = Request::new(
            JsonRpcString::from_code_units(vec![b'm' as u16, 0xd800]),
            None,
            Id::Number(7),
        );

        let response = dispatcher.dispatch(request).await;
        let message = response.error.expect("method-not-found error").message;
        assert_eq!(message, "Method not found");
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
        assert_eq!(resp.result.unwrap().to_value().unwrap(), json!(2));
    }
}
