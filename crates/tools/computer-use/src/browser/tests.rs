use super::*;
use permission::computer_access::ComputerAccessResponse;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use tokio_util::sync::CancellationToken;

static NEXT: AtomicU64 = AtomicU64::new(0);
const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Host {
    path: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Host {
    fn new(responses: Vec<(u16, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let thread = std::thread::spawn(move || {
            for (status, value) in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let (header_end, length) = loop {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
                        let header = String::from_utf8(bytes.clone()).unwrap();
                        assert!(header.starts_with("POST /agent HTTP/1.1\r\n"));
                        assert!(header
                            .to_lowercase()
                            .contains(&format!("authorization: bearer {TOKEN}")));
                        let length = header
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|length| length.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (bytes.len(), length);
                    }
                    assert!(bytes.len() < 16384);
                };
                bytes.resize(header_end + length, 0);
                socket.read_exact(&mut bytes[header_end..]).unwrap();
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&bytes[header_end..]).unwrap());
                let body = serde_json::to_vec(&value).unwrap();
                write!(socket,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let path = std::env::temp_dir().join(format!(
            "lingxi-browser-tool-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        serde_json::to_writer(
            &mut file,
            &json!({"url":format!("http://{address}"),"token":TOKEN}),
        )
        .unwrap();
        Self {
            path,
            requests,
            thread: Some(thread),
        }
    }
    fn tool(&self, resolver: Arc<dyn ComputerAccessResolver>) -> BrowserTool {
        BrowserTool {
            descriptor: self.path.clone(),
            resolver,
            instance_id: lingxi_core::types::SessionId::new().to_string(),
        }
    }
    fn finish(mut self) -> Vec<Value> {
        self.thread.take().unwrap().join().unwrap();
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Resolver {
    answer: ComputerAccessResponse,
    requests: Mutex<Vec<ComputerAccessRequest>>,
}
#[async_trait]
impl ComputerAccessResolver for Resolver {
    async fn resolve(&self, request: ComputerAccessRequest) -> ComputerAccessResponse {
        self.requests.lock().unwrap().push(request);
        self.answer.clone()
    }
}
fn resolver(apps: Vec<String>) -> Arc<Resolver> {
    Arc::new(Resolver {
        answer: ComputerAccessResponse {
            granted_apps: apps,
            ..Default::default()
        },
        requests: Mutex::new(vec![]),
    })
}
fn input(action: &str) -> Utf16JsonProjection {
    super::super::test_projection(
        &json!({"action":action,"reason":"Read the requested page","url":"https://example.test"}),
    )
}
fn ask() -> Value {
    json!({"approval":{"nonce":"opaque-single-use-nonce","label":"Browsing https://example.test","revision":"revision-a"}})
}

#[tokio::test]
async fn approval_is_trusted_scoped_and_not_exposed_to_the_model() {
    let host = Host::new(vec![
        (200, ask()),
        (
            200,
            json!({"result":{"tabId":4,"url":"https://example.test"}}),
        ),
    ]);
    let resolver = resolver(vec!["Browsing https://example.test".into()]);
    let result = host
        .tool(resolver.clone())
        .execute(&input("navigate"), &tool_api::test_support::fresh_ctx())
        .await
        .unwrap();
    let serialized = result.data.to_json_string().unwrap();
    assert!(!serialized.contains("nonce"));
    assert!(!serialized.contains(TOKEN));
    let calls = host.finish();
    assert_eq!(calls.len(), 2);
    assert!(calls[0]["sessionId"].as_str().unwrap().len() < 192);
    assert!(calls[0].get("approvalNonce").is_none());
    let mut retry = calls[1].clone();
    assert_eq!(
        retry.as_object_mut().unwrap().remove("approvalNonce"),
        Some(json!("opaque-single-use-nonce"))
    );
    assert_eq!(
        retry.as_object_mut().unwrap().remove("approved"),
        Some(json!(true))
    );
    assert_eq!(calls[0], retry);
    let requests = resolver.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].apps[0].label, "Browsing https://example.test");
    assert!(
        !requests[0].clipboard_read
            && !requests[0].clipboard_write
            && !requests[0].system_key_combos
    );
}

#[tokio::test]
async fn denial_is_reported_as_denial_even_if_nonce_consumption_is_rejected() {
    let host = Host::new(vec![
        (200, ask()),
        (
            400,
            json!({"error":"Browser approval expired or does not match this request"}),
        ),
    ]);
    let error = host
        .tool(resolver(vec![]))
        .execute(&input("navigate"), &tool_api::test_support::fresh_ctx())
        .await
        .unwrap_err();
    assert_eq!(
        error.model_facing_message().string_at(""),
        Some("Browser access was denied.")
    );
    let calls = host.finish();
    assert_eq!(calls[1]["approved"], false);
}

#[tokio::test]
async fn response_cannot_grant_an_extra_site_or_capability() {
    let host = Host::new(vec![(200, ask()), (200, json!({"error":"Denied"}))]);
    let error = host
        .tool(resolver(vec![
            "Browsing https://example.test".into(),
            "extra".into(),
        ]))
        .execute(&input("navigate"), &tool_api::test_support::fresh_ctx())
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::PermissionDenied(_)));
    assert_eq!(host.finish()[1]["approved"], false);
}

#[tokio::test]
async fn stale_permission_is_not_reprompted_or_retried() {
    let host = Host::new(vec![
        (200, ask()),
        (400, json!({"error":"Browser settings changed"})),
    ]);
    let error = host
        .tool(resolver(vec!["Browsing https://example.test".into()]))
        .execute(&input("navigate"), &tool_api::test_support::fresh_ctx())
        .await
        .unwrap_err();
    assert_eq!(
        error.model_facing_message().string_at(""),
        Some("Browser settings changed")
    );
    assert_eq!(host.finish().len(), 2);
}

#[tokio::test]
async fn cancellation_aborts_a_pending_approval_without_retrying_execution() {
    let host = Host::new(vec![(200, ask())]);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tool = host.tool(Arc::new(super::super::TuiBridgeResolver::new(tx)));
    let mut ctx = tool_api::test_support::fresh_ctx();
    let cancel = CancellationToken::new();
    ctx.cancel = Some(cancel.clone());
    let request = input("navigate");
    let call = tool.execute(&request, &ctx);
    let cancel_when_asked = async {
        let pending = rx.recv().await.unwrap();
        cancel.cancel();
        drop(pending);
    };
    let (result, ()) = tokio::join!(call, cancel_when_asked);
    assert!(matches!(result, Err(ToolError::Aborted)));
    assert_eq!(host.finish().len(), 1);
}

#[tokio::test]
async fn recreating_tool_with_same_conversation_cannot_reuse_host_session_grants() {
    let host = Host::new(vec![
        (200, json!({"result":{"tabs":[]}})),
        (200, json!({"result":{"tabs":[]}})),
    ]);
    let ctx = tool_api::test_support::fresh_ctx();
    host.tool(resolver(vec![]))
        .execute(&input("tabs"), &ctx)
        .await
        .unwrap();
    host.tool(resolver(vec![]))
        .execute(&input("tabs"), &ctx)
        .await
        .unwrap();
    let calls = host.finish();
    assert_ne!(calls[0]["sessionId"], calls[1]["sessionId"]);
}

#[tokio::test]
async fn screenshot_is_a_valid_model_image_not_embedded_base64_text() {
    let mut png = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
    let host = Host::new(vec![(
        200,
        json!({"result":{"tabId":7,"viewport":{"width":2,"height":2,"offsetX":3,"offsetY":4,"pageX":10,"pageY":20},"image":{"data":encoded,"mimeType":"image/png"}}}),
    )]);
    let result = host
        .tool(resolver(vec![]))
        .execute(&input("screenshot"), &tool_api::test_support::fresh_ctx())
        .await
        .unwrap();
    let blocks = tool_api::tool_result_media::image_content_blocks(&result.data).unwrap();
    assert_eq!(blocks.len(), 1);
    assert!(result
        .model_content
        .as_ref()
        .unwrap()
        .string_at("")
        .unwrap()
        .contains("CSS pixels"));
    assert!(!result
        .model_content
        .as_ref()
        .unwrap()
        .string_at("")
        .unwrap()
        .contains("base64"));
    assert_eq!(result.data.string_at("/type"), Some("image"));
    assert_eq!(
        result.data.string_at("/coordinateSpace"),
        Some("CSS pixels in the tab viewport")
    );
    assert_eq!(host.finish().len(), 1);
    assert_eq!(
        result
            .data
            .number_at("/coordinateMapping/cssXPerImagePixel"),
        Some(0.5)
    );
    assert_eq!(
        result
            .data
            .number_at("/coordinateMapping/cssYPerImagePixel"),
        Some(0.5)
    );
    assert_eq!(
        result.data.number_at("/coordinateMapping/offsetX"),
        Some(3.0)
    );
    assert_eq!(
        result.data.number_at("/coordinateMapping/offsetY"),
        Some(4.0)
    );
}

#[test]
fn model_cannot_supply_host_authority_fields() {
    for field in [
        "sessionId",
        "approvalNonce",
        "approved",
        "token",
        "revision",
    ] {
        let value = super::super::test_projection(
            &json!({"action":"navigate","reason":"Task",(field):"forged"}),
        );
        assert!(BrowserTool::input(&value).is_err(), "{field}");
    }
}

#[test]
fn retina_capture_mapping_uses_the_actual_downsampled_model_image() {
    let mut png = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgb8(image::RgbImage::new(4000, 2000))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let processed = tool_api::util::image_budget::process_image(png.into_inner()).unwrap();
    assert_eq!(processed.dimensions, (2000, 1000));
    let mapping = coordinate_mapping(
        &json!({"width":2000,"height":1000,"pageX":900,"pageY":600}),
        processed.dimensions,
    )
    .unwrap();
    // Retina2x followed by model2x shrink cancels; document scrolling is not
    // an input viewport offset and must not be added to a click.
    assert_eq!(mapping["cssXPerImagePixel"], 1.0);
    assert_eq!(mapping["cssYPerImagePixel"], 1.0);
    assert_eq!(mapping["offsetX"], 0.0);
    assert_eq!(mapping["offsetY"], 0.0);
}

#[test]
fn mapping_requires_real_finite_viewport_geometry() {
    for viewport in [
        json!(null),
        json!({"width":0,"height":2}),
        json!({"width":2,"height":-1}),
        json!({"width":2,"height":2,"offsetX":"bad"}),
    ] {
        assert!(coordinate_mapping(&viewport, (4, 4)).is_err());
    }
}

#[test]
fn descriptor_rejects_non_loopback_and_world_readable_files() {
    let host = Host::new(vec![]);
    std::fs::write(
        &host.path,
        serde_json::to_vec(&json!({"url":"http://example.test:1234","token":TOKEN})).unwrap(),
    )
    .unwrap();
    assert!(read_descriptor(&host.path).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            &host.path,
            serde_json::to_vec(&json!({"url":"http://127.0.0.1:1234","token":TOKEN})).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&host.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_descriptor(&host.path).is_err());
    }
    host.finish();
}
