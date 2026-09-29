//! `reqwest`-backed [`HttpTransport`] for desktop hosts.
//!
//! The real client now lives in `http_client::ReqwestHttp` — the
//! single source of truth shared by all hosts. This module re-exports it
//! under the historical `PosixHttp` name so `platform_posix`'s public API
//! and behavior are byte-identical to before the extraction.
//!
//! `lingxi_core::host::HttpTransport` is implemented on `ReqwestHttp`, so the re-exported
//! `PosixHttp` satisfies the same trait bound with no wrapper indirection.

/// Production HTTP transport using `reqwest::Client`.
///
/// Type alias to the shared [`http_client::ReqwestHttp`]; preserves
/// the `platform_posix::PosixHttp` path that desktop callers and tests use.
pub use http_client::ReqwestHttp as PosixHttp;
