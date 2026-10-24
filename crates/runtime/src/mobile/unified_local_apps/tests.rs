use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn service() -> (tempfile::TempDir, Arc<MobileLocalAppServiceHandle>) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("user/.lingxi");
    let service = MobileLocalAppServiceHandle::new(home, None, "ios".into()).unwrap();
    (dir, service)
}
async fn publish(service: &Arc<MobileLocalAppServiceHandle>, dir: &Path, name: &str) -> Value {
    let source = dir.join(name);
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("index.html"), "hello independent app").unwrap();
    std::fs::write(source.join("lingxi-app.json"),json!({"schema":2,"name":name,"entry":{"type":"static","path":"index.html"},"targets":[{"platform":"ios","environment":"web"}]}).to_string()).unwrap();
    service
        .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":source}))
        .await
        .unwrap()
}
pub(super) async fn get(port: u16, path: &str) -> String {
    let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    socket
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).await.unwrap();
    String::from_utf8(bytes).unwrap()
}
#[tokio::test]
async fn static_app_runs_without_linux_and_restart_keeps_origin_but_revokes_epoch() {
    let (dir, service) = service();
    let published = publish(&service, dir.path(), "static").await;
    let id = published["record"]["appId"].as_str().unwrap();
    let port = published["record"]["originPort"].as_u64().unwrap() as u16;
    let first = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    let first_run = first["run"]["runId"].as_str().unwrap();
    assert!(get(port, &format!("/__run/{first_run}/"))
        .await
        .ends_with("hello independent app"));
    service
        .request_value(json!({"op":"suspend","appId":id}))
        .await
        .unwrap();
    assert_eq!(
        service
            .request_value(json!({"op":"runs","appId":id}))
            .await
            .unwrap()[0]["phase"],
        "suspended"
    );
    let second = service
        .request_value(json!({"op":"resume","appId":id}))
        .await
        .unwrap();
    assert_eq!(first["origin"], second["origin"]);
    assert_ne!(first["run"]["runId"], second["run"]["runId"]);
    assert!(get(port, &format!("/__run/{first_run}/"))
        .await
        .starts_with("HTTP/1.1 410"));
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}
#[tokio::test]
async fn port_conflict_preserves_origin_and_release() {
    let (dir, service) = service();
    let published = publish(&service, dir.path(), "conflict").await;
    let id = published["record"]["appId"].as_str().unwrap();
    let port = published["record"]["originPort"].as_u64().unwrap() as u16;
    let occupied = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let error = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "originPortUnavailable");
    let current = service
        .request_value(json!({"op":"get","appId":id}))
        .await
        .unwrap();
    assert_eq!(current["originPort"], port);
    assert_eq!(current["currentRevision"], 1);
    drop(occupied);
    service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn activation_write_failure_keeps_previous_listener_run_and_release() {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(PathBuf, std::fs::Permissions);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, self.1.clone());
        }
    }
    let (dir, service) = service();
    let published = publish(&service, dir.path(), "commit-failure").await;
    let id = published["record"]["appId"].as_str().unwrap();
    let port = published["record"]["originPort"].as_u64().unwrap() as u16;
    let first = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    let old = first["run"]["runId"].as_str().unwrap();
    std::fs::write(dir.path().join("commit-failure/index.html"), "replacement").unwrap();
    service.request_value(json!({"op":"publish","appId":id,"expectedRevision":1,"sourcePath":dir.path().join("commit-failure")})).await.unwrap();
    let app = service.library.root().join("apps").join(id);
    let restore = Restore(app.clone(), std::fs::metadata(&app).unwrap().permissions());
    // run/preparation subdirectories remain writable; the final atomic
    // record.json rename fails, modeling a commit-stage storage failure.
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(service
        .request_value(json!({"op":"start","appId":id,"revision":2}))
        .await
        .is_err());
    drop(restore);
    assert_eq!(
        service
            .request_value(json!({"op":"get","appId":id}))
            .await
            .unwrap()["currentRevision"],
        1
    );
    assert_eq!(
        service
            .request_value(json!({"op":"status","appId":id}))
            .await
            .unwrap()["run"]["runId"],
        old
    );
    assert!(get(port, &format!("/__run/{old}/"))
        .await
        .ends_with("hello independent app"));
    assert!(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .is_err());
    let second = service
        .request_value(json!({"op":"start","appId":id,"revision":2}))
        .await
        .unwrap();
    assert_ne!(second["run"]["runId"], first["run"]["runId"]);
    assert!(get(port, &format!("/__run/{old}/"))
        .await
        .starts_with("HTTP/1.1 410"));
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
    // Watchers can retain ActiveRun Arc values; none may retain the listener.
    assert!(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .is_ok());
}
#[tokio::test]
async fn execution_release_requires_consent_before_runtime_is_used() {
    let (dir, service) = service();
    let source = dir.path().join("server");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("lingxi-app.json"),json!({"schema":2,"name":"server","entry":{"type":"server","process":"api"},"install":[{"command":"echo installing"}],"processes":[{"name":"api","command":"python3 server.py"}],"targets":[{"platform":"ios","environment":"mobile-linux"}],"capabilities":["process"]}).to_string()).unwrap();
    let published = service
        .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":source}))
        .await
        .unwrap();
    let id = published["record"]["appId"].as_str().unwrap();
    service.request_value(json!({"op":"configureTarget","target":{"platform":"ios","environment":"mobile-linux","capabilities":["process"],"channels":["development"]}})).await.unwrap();
    let denied = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap_err();
    assert_eq!(denied.code, "consentRequired");
    service
        .request_value(json!({"op":"grant","appId":id,"digest":published["release"]["digest"]}))
        .await
        .unwrap();
    let unavailable = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap_err();
    assert_eq!(unavailable.code, "runtimeUnavailable");
    assert_eq!(
        service
            .request_value(json!({"op":"get","appId":id}))
            .await
            .unwrap()["currentRevision"],
        1
    );
}
#[tokio::test]
async fn two_apps_run_independently_and_same_home_reuses_process_owner() {
    let (dir, service) = service();
    let a = publish(&service, dir.path(), "one").await;
    let b = publish(&service, dir.path(), "two").await;
    let a_id = a["record"]["appId"].as_str().unwrap();
    let b_id = b["record"]["appId"].as_str().unwrap();
    let first = service
        .request_value(json!({"op":"start","appId":a_id}))
        .await
        .unwrap();
    let second = service
        .request_value(json!({"op":"start","appId":b_id}))
        .await
        .unwrap();
    assert_ne!(first["origin"], second["origin"]);
    let reused =
        MobileLocalAppServiceHandle::new(service.config_home.clone(), None, "ios".into()).unwrap();
    assert!(Arc::ptr_eq(&service, &reused));
    reused
        .request_value(json!({"op":"stop","appId":a_id}))
        .await
        .unwrap();
    assert_eq!(
        service
            .request_value(json!({"op":"status","appId":b_id}))
            .await
            .unwrap()["run"]["phase"],
        "running"
    );
    service
        .request_value(json!({"op":"stop","appId":b_id}))
        .await
        .unwrap();
}
#[tokio::test]
async fn ffi_errors_are_explicit_and_ui_cannot_activate_without_runtime() {
    let (_dir, service) = service();
    let response: Value =
        serde_json::from_str(&service.clone().request_json("{invalid".into()).await).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "invalidRequest");
    let denied = service
        .request_value(json!({"op":"activateRelease","appId":"id","revision":1}))
        .await
        .unwrap_err();
    assert_eq!(denied.code, "invalidOperation");
}

#[tokio::test]
async fn nested_built_entry_keeps_relative_asset_base() {
    let (dir, service) = service();
    let source = dir.path().join("nested");
    std::fs::create_dir_all(source.join("dist/assets")).unwrap();
    std::fs::write(
        source.join("dist/index.html"),
        "<script src=\"./assets/app.js\"></script>",
    )
    .unwrap();
    std::fs::write(source.join("dist/assets/app.js"), "fixture").unwrap();
    std::fs::write(source.join("lingxi-app.json"),json!({"schema":2,"name":"nested","entry":{"type":"static","path":"dist/index.html"},"targets":[{"platform":"ios","environment":"web"}]}).to_string()).unwrap();
    let published = service
        .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":source}))
        .await
        .unwrap();
    let id = published["record"]["appId"].as_str().unwrap();
    let port = published["record"]["originPort"].as_u64().unwrap() as u16;
    let started = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    assert!(started["url"]
        .as_str()
        .unwrap()
        .ends_with("/dist/index.html"));
    let run_id = started["run"]["runId"].as_str().unwrap();
    assert!(get(port, &format!("/__run/{run_id}/dist/assets/app.js"))
        .await
        .ends_with("fixture"));
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}

#[tokio::test]
async fn superseded_run_cleanup_cannot_stop_the_new_run() {
    let (dir, service) = service();
    let published = publish(&service, dir.path(), "superseded").await;
    let id = published["record"]["appId"].as_str().unwrap();
    service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    let old = service.active.lock().await.get(id).unwrap().clone();
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
    let current = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    service
        .stop_owned(id, old, RunPhase::Failed, false)
        .await
        .unwrap();
    let status = service
        .request_value(json!({"op":"status","appId":id}))
        .await
        .unwrap();
    assert_eq!(status["run"]["runId"], current["run"]["runId"]);
    assert_eq!(status["run"]["phase"], "running");
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}
#[tokio::test]
async fn runtime_requests_reject_unknown_fields_and_invalid_revisions() {
    let (_dir, service) = service();
    assert_eq!(
        service
            .request_value(json!({"op":"start","appId":"abc","revision":"latest"}))
            .await
            .unwrap_err()
            .code,
        "invalidRequest"
    );
    assert_eq!(
        service
            .request_value(json!({"op":"start","appId":"abc","ignoreConsent":true}))
            .await
            .unwrap_err()
            .code,
        "invalidRequest"
    );
}

#[tokio::test]
async fn update_failure_keeps_previous_running_release_and_success_switches_epoch() {
    let (dir, service) = service();
    let original = publish(&service, dir.path(), "updatable").await;
    let id = original["record"]["appId"].as_str().unwrap();
    let port = original["record"]["originPort"].as_u64().unwrap() as u16;
    service.request_value(json!({"op":"configureTarget","target":{"platform":"ios","environment":"mobile-linux","capabilities":["process"]}})).await.unwrap();
    let before = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    let old_run = before["run"]["runId"].as_str().unwrap();
    // Repeating a native capability handshake is allowed while running.
    service.request_value(json!({"op":"configureTarget","target":{"platform":"ios","environment":"mobile-linux","capabilities":["process"]}})).await.unwrap();
    let source = dir.path().join("updatable");
    std::fs::write(source.join("lingxi-app.json"),json!({"schema":2,"name":"updatable","entry":{"type":"server","process":"api"},"processes":[{"name":"api","command":"python3 server.py"}],"targets":[{"platform":"ios","environment":"mobile-linux"}],"capabilities":["process"]}).to_string()).unwrap();
    let staged = service
        .request_value(json!({"op":"publish","appId":id,"expectedRevision":1,"sourcePath":source}))
        .await
        .unwrap();
    service
        .request_value(json!({"op":"grant","appId":id,"digest":staged["release"]["digest"]}))
        .await
        .unwrap();
    assert_eq!(
        service
            .request_value(json!({"op":"start","appId":id,"revision":2}))
            .await
            .unwrap_err()
            .code,
        "runtimeUnavailable"
    );
    assert_eq!(
        service
            .request_value(json!({"op":"status","appId":id}))
            .await
            .unwrap()["run"]["runId"],
        old_run
    );
    assert!(get(port, &format!("/__run/{old_run}/"))
        .await
        .ends_with("hello independent app"));
    assert_eq!(
        service
            .request_value(json!({"op":"get","appId":id}))
            .await
            .unwrap()["currentRevision"],
        1
    );
    std::fs::write(source.join("lingxi-app.json"),json!({"schema":2,"name":"updated","entry":{"type":"static","path":"index.html"},"targets":[{"platform":"ios","environment":"web"}]}).to_string()).unwrap();
    std::fs::write(source.join("index.html"), "updated app").unwrap();
    service
        .request_value(json!({"op":"publish","appId":id,"expectedRevision":2,"sourcePath":source}))
        .await
        .unwrap();
    let after = service
        .request_value(json!({"op":"start","appId":id,"revision":3}))
        .await
        .unwrap();
    assert_eq!(before["origin"], after["origin"]);
    assert_ne!(before["run"]["runId"], after["run"]["runId"]);
    assert!(get(port, &format!("/__run/{old_run}/"))
        .await
        .starts_with("HTTP/1.1 410"));
    assert_eq!(
        service
            .request_value(json!({"op":"get","appId":id}))
            .await
            .unwrap()["currentRevision"],
        3
    );
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}

#[tokio::test]
async fn data_schema_change_does_not_prepare_or_replace_the_live_application() {
    let (dir, service) = service();
    let original = publish(&service, dir.path(), "data-schema").await;
    let id = original["record"]["appId"].as_str().unwrap();
    let before = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    let source = dir.path().join("data-schema");
    std::fs::write(source.join("lingxi-app.json"),json!({"schema":2,"name":"data-schema","entry":{"type":"static","path":"index.html"},"targets":[{"platform":"ios","environment":"web"}],"dataSchema":{"version":2,"compatibleFrom":1}}).to_string()).unwrap();
    service
        .request_value(json!({"op":"publish","appId":id,"expectedRevision":1,"sourcePath":source}))
        .await
        .unwrap();
    assert_eq!(
        service
            .request_value(json!({"op":"start","appId":id,"revision":2}))
            .await
            .unwrap_err()
            .code,
        "dataMigrationRequired"
    );
    let status = service
        .request_value(json!({"op":"status","appId":id}))
        .await
        .unwrap();
    assert_eq!(status["run"]["runId"], before["run"]["runId"]);
    assert_eq!(status["run"]["phase"], "running");
    assert_eq!(
        service
            .request_value(json!({"op":"runs","appId":id}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}

#[tokio::test]
async fn external_browser_sdk_and_agent_route_bind_caller_to_the_current_epoch() {
    let (dir, service) = service();
    let published = publish(&service, dir.path(), "browser").await;
    let id = published["record"]["appId"].as_str().unwrap();
    let port = published["record"]["originPort"].as_u64().unwrap() as u16;
    let launch = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    let epoch = launch["run"]["runId"].as_str().unwrap();
    let sdk = get(port, &format!("/__run/{epoch}/__host/sdk.js")).await;
    assert!(sdk.starts_with("HTTP/1.1 200"));
    assert!(sdk.contains(id));
    assert!(!sdk.contains("__LINGXI_APP_CONFIGURATION__"));
    assert!(sdk.contains("revision:config.revision"));
    async fn post(
        port: u16,
        epoch: &str,
        origin: bool,
        referer_epoch: &str,
        method: &str,
        extra: &str,
    ) -> String {
        let body = format!(r#"{{"method":"{method}","args":{{}}{extra}}}"#);
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let origin = if origin {
            format!("Origin: http://127.0.0.1:{port}\r\n")
        } else {
            String::new()
        };
        socket.write_all(format!("POST /__run/{epoch}/__host/agent HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{origin}Referer: http://127.0.0.1:{port}/__run/{referer_epoch}/index.html\r\nContent-Type: application/json\r\nX-LingXi-App-Request: 1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        let mut out = Vec::new();
        socket.read_to_end(&mut out).await.unwrap();
        String::from_utf8(out).unwrap()
    }
    assert!(post(port, epoch, false, epoch, "agent.run", "")
        .await
        .starts_with("HTTP/1.1 403"));
    assert!(post(port, epoch, true, "stale", "agent.run", "")
        .await
        .starts_with("HTTP/1.1 403"));
    assert!(post(port, epoch, true, epoch, "answer", "")
        .await
        .starts_with("HTTP/1.1 403"));
    assert!(post(
        port,
        epoch,
        true,
        epoch,
        "agent.run",
        r#", "appId":"other""#
    )
    .await
    .starts_with("HTTP/1.1 400"));
    let no_model = post(port, epoch, true, epoch, "agent.run", "").await;
    assert!(no_model.starts_with("HTTP/1.1 200"));
    assert!(no_model.contains("agent_model_host_not_ready"));
    service
        .request_value(json!({"op":"restart","appId":id}))
        .await
        .unwrap();
    assert!(post(port, epoch, true, epoch, "agent.run", "")
        .await
        .starts_with("HTTP/1.1 410"));
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}
