//! The cli-demo / mobile-minimal host now uses the real shared transport
//! (`http-client`, `reqwest` + `rustls-tls`) instead of the former erroring
//! stub. Re-exported under the historical `PosixHttp` name.

pub use http_client::ReqwestHttp as PosixHttp;
