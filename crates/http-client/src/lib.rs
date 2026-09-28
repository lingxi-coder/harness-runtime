//! Shared pure-Rust HTTP transport (`reqwest` + `rustls-tls`).
//!
//! Single source of truth for the production [`platform_api::http::HttpTransport`]
//! used by every host (desktop, windows, mobile). Replaces the formerly
//! duplicated `platforms/{common,windows,posix}` copies.

pub mod reqwest_http;
mod tls_config;

pub use reqwest_http::ReqwestHttp;

mod monitor_websocket;

/// Construct provider networking using the SDK; general-purpose HTTP stays separate.
pub fn provider_transport(
) -> Result<lingxi_llm_client::HttpTransport, lingxi_llm_client::protocol::LlmError> {
    let tls = tls_config::TlsSettings::from_env();
    lingxi_llm_client::HttpTransport::with_client_configurator(|builder| {
        tls.apply_to_builder(builder.connect_timeout(std::time::Duration::from_secs(10)))
    })
}
