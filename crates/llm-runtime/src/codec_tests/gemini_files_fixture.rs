//! Private test envelope adapter for SDK Gemini file requests.
use crate::{LlmError, ProviderRequest};
use lingxi_llm_client::providers::google::files_wire as wire;
pub use wire::GeminiFile;
fn project(request: lingxi_llm_client::HttpRequest) -> ProviderRequest {
    let mut host = ProviderRequest::post_json(
        request.url,
        lingxi_llm_client::protocol::JsonPayload::default(),
    );
    host.method = request.method;
    host.headers = request.headers.into_iter().collect();
    if host
        .headers
        .get("content-type")
        .is_some_and(|value| value == "application/json")
    {
        let source = std::str::from_utf8(&request.body).expect("upstream JSON is UTF-8");
        host.body_json = lingxi_llm_client::protocol::JsonPayload::parse(source)
            .expect("upstream generated JSON");
        host.body_bytes = Some(request.body.to_vec());
    } else if !request.body.is_empty() {
        host.body_bytes = Some(request.body.to_vec());
    }
    host
}
pub fn start_upload_request(base: &str, size: usize, mime: &str, name: &str) -> ProviderRequest {
    project(wire::start_upload_request(base, size, mime, name))
}
pub fn upload_finalize_request(url: &str, bytes: Vec<u8>) -> ProviderRequest {
    project(wire::upload_finalize_request(url, bytes))
}
pub fn file_status_request(base: &str, name: &str) -> ProviderRequest {
    project(wire::file_status_request(base, name))
}
pub fn parse_start_response(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<String, LlmError> {
    wire::parse_start_response(headers).map_err(crate::upstream::error)
}
pub fn parse_upload_response(body: &serde_json::Value) -> Result<GeminiFile, LlmError> {
    wire::parse_upload_response(body).map_err(crate::upstream::error)
}
pub fn parse_file_status(body: &serde_json::Value) -> Result<GeminiFile, LlmError> {
    wire::parse_file_status(body).map_err(crate::upstream::error)
}
