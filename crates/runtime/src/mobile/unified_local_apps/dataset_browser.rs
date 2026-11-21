use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

use super::{MobileLocalAppServiceHandle, RunPhase, ServiceError};

impl MobileLocalAppServiceHandle {
    pub(super) async fn dataset_bootstrap(
        self: &Arc<Self>,
        app_id: &str,
        revision: u64,
        bridge_origin: &str,
        proof: &str,
    ) -> Result<String, ServiceError> {
        let bridge = Url::parse(bridge_origin).map_err(|_| invalid())?;
        if bridge.scheme() != "https"
            || bridge_origin.trim_end_matches('/') != bridge.origin().ascii_serialization()
            || proof.len() != 43
            || !proof
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(invalid());
        }
        let run = self
            .active
            .lock()
            .await
            .get(app_id)
            .cloned()
            .ok_or_else(invalid)?;
        {
            let record = run.record.lock().map_err(|_| invalid())?;
            if record.phase != RunPhase::Running
                || record.revision != revision
                || run.cancel.is_cancelled()
            {
                return Err(invalid());
            }
        }
        let release = self.library.release(app_id, revision)?;
        if release.manifest.data_bindings.is_empty() {
            return Err(invalid());
        }
        let app_url = run.url.lock().map_err(|_| invalid())?.clone();
        if app_url.is_empty() {
            return Err(invalid());
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let host = listener.local_addr()?.to_string();
        let route = format!("/{}", uuid::Uuid::new_v4().simple());
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let destination = bridge
            .join("v1/dataset-browser/launch")
            .map_err(|_| invalid())?;
        let html = bootstrap_html(destination.as_str(), &app_url, proof, &nonce);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; form-action {}; frame-ancestors 'none'; script-src 'nonce-{}'; base-uri 'none'\r\n\r\n{}",
            html.len(), bridge.origin().ascii_serialization(), nonce, html
        );
        *run.dataset_bridge_origin.lock().map_err(|_| invalid())? =
            Some(bridge.origin().ascii_serialization());
        let address = format!("http://{host}{route}");
        let cancel = run.cancel.clone();
        tokio::spawn(async move {
            let serve = async {
                while let Ok((mut socket, _)) = listener.accept().await {
                    if tokio::time::timeout(
                        Duration::from_secs(3),
                        bootstrap_request(&mut socket, &host, &route),
                    )
                    .await
                    .unwrap_or(false)
                    {
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        break;
                    }
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::timeout(Duration::from_secs(35), serve) => {}
            }
        });
        Ok(address)
    }
}

fn invalid() -> ServiceError {
    ServiceError::new(
        "dataset_browser_unavailable",
        "The Local App Dataset handoff is invalid or expired.",
    )
}

fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn bootstrap_html(destination: &str, app_url: &str, proof: &str, nonce: &str) -> String {
    let destination = escaped(destination);
    let app_url = escaped(app_url);
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>LingXi Local App</title></head>
<body><form method="post" action="{destination}"><input type="hidden" name="proof" value="{proof}">
<input type="hidden" name="appUrl" value="{app_url}"><button type="submit">Open Local App</button>
</form><script nonce="{nonce}">document.forms[0].submit()</script></body></html>"#
    )
}

async fn bootstrap_request(socket: &mut TcpStream, host: &str, route: &str) -> bool {
    let mut bytes = Vec::new();
    while bytes.len() < 8192 {
        let mut chunk = [0_u8; 1024];
        let Ok(count) = socket.read(&mut chunk).await else {
            return false;
        };
        if count == 0 {
            return false;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut request = httparse::Request::new(&mut headers);
            if !matches!(request.parse(&bytes), Ok(httparse::Status::Complete(_))) {
                return false;
            }
            let mut hosts = request
                .headers
                .iter()
                .filter(|header| header.name.eq_ignore_ascii_case("host"));
            return request.method == Some("GET")
                && request.path == Some(route)
                && hosts
                    .next()
                    .is_some_and(|header| header.value == host.as_bytes())
                && hosts.next().is_none();
        }
    }
    false
}
