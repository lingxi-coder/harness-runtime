//! Re-export shim: Windows uses the shared `http-client` transport
//! (`reqwest` + `rustls-tls`), identical to every other host. The former
//! duplicate `WindowsHttp` impl was deleted in the http-client unification.

/// Production HTTP transport. Alias to the shared [`http_client::ReqwestHttp`];
/// preserves the historical `platform_windows::http::WindowsHttp` path.
pub use http_client::ReqwestHttp as WindowsHttp;
