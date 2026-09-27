//! Re-export shim: the real `ReqwestHttp` now lives in the shared `http-client`
//! crate. Kept so `platform_common::http::ReqwestHttp` stays a valid path.

pub use http_client::ReqwestHttp;
