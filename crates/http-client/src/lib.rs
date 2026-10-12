//! Shared pure-Rust HTTP transport (`reqwest` + `rustls-tls`).
//!
//! Single source of truth for the production [`lingxi_core::host::http::HttpTransport`]
//! used by every host (desktop, windows, mobile). Replaces the formerly
//! duplicated `platforms/{common,windows,posix}` copies.

pub mod reqwest_http;
mod tls_config;

pub use reqwest_http::ReqwestHttp;

mod monitor_websocket;
mod native_proxy;

/// Construct provider networking using the SDK; general-purpose HTTP stays separate.
pub fn provider_transport(
) -> Result<lingxi_llm_client::HttpTransport, lingxi_llm_client::protocol::LlmError> {
    let tls = tls_config::TlsSettings::from_env();
    build_provider_transport(None, tls)
}

/// Construct provider networking with a second, request-scoped Native Claude
/// proxy pool. The default pool retains reqwest's existing proxy behavior for
/// every other provider; the SDK selects the Native pool only for first-party
/// Claude/Foundry requests.
pub fn provider_transport_with_proxy_auth_helper(
    helper: std::sync::Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>,
) -> Result<lingxi_llm_client::HttpTransport, lingxi_llm_client::protocol::LlmError> {
    let tls = tls_config::TlsSettings::from_env();
    build_provider_transport(Some(helper), tls)
}

fn build_provider_transport(
    helper: Option<std::sync::Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>>,
    tls: tls_config::TlsSettings,
) -> Result<lingxi_llm_client::HttpTransport, lingxi_llm_client::protocol::LlmError> {
    let configure_default = |builder: lingxi_llm_client::transport::http_backend::ClientBuilder| {
        tls.apply_to_builder(builder.connect_timeout(std::time::Duration::from_secs(10)))
    };
    let configure_native = |builder: lingxi_llm_client::transport::http_backend::ClientBuilder,
                            helper: Option<
        std::sync::Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>,
    >| {
        let builder =
            tls.apply_to_builder(builder.connect_timeout(std::time::Duration::from_secs(10)));
        native_proxy::install_process_proxy(builder, helper)
    };
    lingxi_llm_client::HttpTransport::with_read_timeout_and_native_proxy(
        None,
        helper,
        configure_default,
        configure_native,
    )
}
