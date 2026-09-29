//! Model networking uses the SDK contract directly.
pub use lingxi_llm_client::Transport;
use std::{future::Future, pin::Pin};
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
