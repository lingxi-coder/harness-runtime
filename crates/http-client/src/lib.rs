//! Shared pure-Rust HTTP transport (`reqwest` + `rustls-tls`).
//!
//! Single source of truth for the production [`platform_api::http::HttpTransport`]
//! used by every host (desktop, windows, mobile). Replaces the formerly
//! duplicated `platforms/{common,windows,posix}` copies.

pub mod reqwest_http;
mod tls_config;

pub use reqwest_http::ReqwestHttp;

mod monitor_websocket;
