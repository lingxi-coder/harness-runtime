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

async fn native(service: &Arc<MobileLocalAppServiceHandle>, request: Value) -> Value {
    serde_json::from_str(&service.clone().agent_host_json(request.to_string()).await).unwrap()
}

fn market_remix_fixture(service: &MobileLocalAppServiceHandle) -> (Value, Vec<u8>) {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let id = uuid::Uuid::new_v4().to_string();
    let path = service
        .config_home
        .join("local-app-imports")
        .join(format!("market-{id}"))
        .join("source.zip");
    let manifest = json!({"schema":2,"name":"Market fixture","dataSchema":null,
        "entry":{"type":"static","path":"index.html"},"targets":[{"platform":"ios","environment":"web"}],
        "remix":{"allowed":true,"license":"MIT"}});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let html = b"<!doctype html><html>market fixture</html>";
    let gitignore = b"build/\n";
    let well_known = b"Contact: security@example.test\n";
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o600);
    zip.start_file("lingxi-app.json", options).unwrap();
    zip.write_all(&manifest_bytes).unwrap();
    zip.start_file("index.html", options).unwrap();
    zip.write_all(html).unwrap();
    zip.start_file(".gitignore", options).unwrap();
    zip.write_all(gitignore).unwrap();
    zip.start_file(".well-known/security.txt", options).unwrap();
    zip.write_all(well_known).unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    let files = json!([
        {"path":"lingxi-app.json","sizeBytes":manifest_bytes.len(),"sha256":hash(&manifest_bytes),"executable":false},
        {"path":"index.html","sizeBytes":html.len(),"sha256":hash(html),"executable":false},
        {"path":".gitignore","sizeBytes":gitignore.len(),"sha256":hash(gitignore),"executable":false},
        {"path":".well-known/security.txt","sizeBytes":well_known.len(),"sha256":hash(well_known),"executable":false}
    ]);
    let dependencies = json!([]);
    let archive_sha = hash(&bytes);
    let digest =
        local_app::v2::market_release_digest(&archive_sha, &manifest, &files, &dependencies)
            .unwrap();
    (
        json!({"op":"importRemixSource","requestId":id,"archivePath":path,"archiveSha256":archive_sha,
        "archiveBytes":bytes.len(),"manifest":manifest,"files":files,"dependencies":dependencies,
        "marketOrigin":{"appId":"market-app","releaseId":"v1","digest":digest}}),
        bytes,
    )
}

#[tokio::test]
async fn native_authoring_wire_fixture_uses_actual_handlers() {
    let (_dir, service) = service();
    let request_id = uuid::Uuid::new_v4().to_string();
    let create = json!({"op":"createAuthoring","requestId":request_id,"name":"Contract fixture"});
    let created = native(&service, create.clone()).await;
    assert_eq!(created["ok"], true, "{created}");
    let source_id = created["value"]["sourceId"].as_str().unwrap();
    let source = created["value"]["sourceDirectory"].as_str().unwrap();
    let source_path = created["value"]["sourcePath"].as_str().unwrap();
    let proof_request =
        json!({"op":"prepareAuthoringSource","sourceId":source_id,"sourceDirectory":source});
    let proof = native(&service, proof_request.clone()).await;
    let project = uuid::Uuid::new_v4().to_string();
    let workspace = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    let authoring = json!({"sourceDirectory":source,"workspaceId":workspace,
        "projectRef":{"kind":"mobileProject","projectId":project},"authoringSessionId":session});
    let register_request = json!({"op":"registerAuthoring","requestId":request_id,"sourceId":source_id,
        "sessionMode":"code","setCurrent":true,"authoring":authoring});
    let registered = native(&service, register_request.clone()).await;
    assert_eq!(registered["ok"], true, "{registered}");
    assert_eq!(registered["value"]["sourceId"], source_id);
    let lookup_request = json!({"op":"getAuthoringGrant","sourceDirectory":source,"projectId":project,
        "authoringSessionId":session,"sessionMode":"code"});
    let lookup = native(&service, lookup_request.clone()).await;
    assert_eq!(lookup["value"]["sessionMode"], "code");
    assert_eq!(lookup["value"]["sourceId"], source_id);
    let published = service
        .publish_from_trusted_source(
            source_path,
            Some(source_path),
            None,
            0,
            uuid::Uuid::new_v4().to_string(),
            session.clone(),
            Arc::new(Mutex::new(session.clone())),
        )
        .await
        .unwrap();
    let app_id = published["record"]["appId"].as_str().unwrap();
    let prepare_request = json!({"op":"prepareAuthoring","appId":app_id,"sessionBindings":{
        (session.clone()): {"workspaceId":workspace,"sourceDirectory":source,
            "projectRef":{"kind":"mobileProject","projectId":project},"mode":"code"}}});
    let prepare: Value = serde_json::from_str(
        &service
            .clone()
            .request_json(prepare_request.to_string())
            .await,
    )
    .unwrap();
    assert_eq!(prepare["value"]["sourceId"], source_id);
    let moved = Path::new(source_path).with_extension("fixture-saved");
    std::fs::rename(source_path, moved).unwrap();
    let missing: Value = serde_json::from_str(
        &service
            .clone()
            .request_json(prepare_request.to_string())
            .await,
    )
    .unwrap();
    assert_eq!(missing["value"]["action"], "sourceMissing");
    assert!(missing["value"].get("sourceId").is_none());
    let remix_source = _dir.path().join("remix-original");
    std::fs::create_dir(&remix_source).unwrap();
    std::fs::write(remix_source.join("index.html"), "source").unwrap();
    std::fs::write(remix_source.join("lingxi-app.json"), json!({"schema":2,"name":"Original",
        "entry":{"type":"static","path":"index.html"},"targets":[{"platform":"ios","environment":"web"}],
        "remix":{"allowed":true,"license":"MIT"}}).to_string()).unwrap();
    let remix_original = service
        .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":remix_source}))
        .await
        .unwrap();
    let remix_request = json!({"op":"remixAuthoringSource","requestId":uuid::Uuid::new_v4().to_string(),
        "appId":remix_original["record"]["appId"],"revision":1});
    let remix = native(&service, remix_request.clone()).await;
    assert_eq!(remix["ok"], true, "{remix}");
    assert_eq!(
        remix["value"]["record"]["appId"],
        remix["value"]["release"]["appId"]
    );
    let (market_request, market_archive) = market_remix_fixture(&service);
    let market_missing = native(&service, market_request.clone()).await;
    assert_eq!(market_missing["error"]["code"], "archiveMissing");
    let market_path = Path::new(market_request["archivePath"].as_str().unwrap());
    std::fs::create_dir_all(market_path.parent().unwrap()).unwrap();
    std::fs::write(market_path, market_archive).unwrap();
    let market_import = native(&service, market_request.clone()).await;
    assert_eq!(market_import["ok"], true, "{market_import}");
    std::fs::remove_file(market_path).unwrap();
    let market_replay = native(&service, market_request.clone()).await;
    assert_eq!(market_replay, market_import);
    let fixture = json!({"schema":1,"create":{"request":create,"response":created},
        "sourceProof":{"request":proof_request,"response":proof},
        "register":{"request":register_request,"response":registered},
        "get":{"request":lookup_request,"response":lookup},
        "prepare":{"request":prepare_request,"response":prepare},
        "sourceMissing":{"request":prepare_request,"response":missing},
        "installedRemix":{"request":remix_request,"response":remix},
        "marketMissing":{"request":market_request,"response":market_missing},
        "marketImport":{"request":market_request,"response":market_import},
        "marketReplay":{"request":market_request,"response":market_replay}});
    if let Some(path) = std::env::var_os("LINGXI_AUTHORING_FIXTURE_OUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&fixture).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn native_authoring_long_unicode_name_keeps_display_and_reuses_source() {
    let (_dir, service) = service();
    let name = "𐐀".repeat(64);
    let request =
        json!({"op":"createAuthoring","requestId":uuid::Uuid::new_v4().to_string(),"name":name});
    let first = native(&service, request.clone()).await;
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(native(&service, request).await, first);
    let path = Path::new(first["value"]["sourcePath"].as_str().unwrap());
    assert!(path.file_name().unwrap().to_string_lossy().len() < 255);
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(path.join("lingxi-app.json")).unwrap()).unwrap();
    assert_eq!(manifest["name"], name);
}

#[tokio::test]
async fn native_installed_remix_creates_a_new_owned_source_and_replays() {
    let (dir, service) = service();
    let source = dir.path().join("original");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("index.html"), "source").unwrap();
    std::fs::write(source.join("lingxi-app.json"), json!({"schema":2,"name":"Original",
        "entry":{"type":"static","path":"index.html"},"targets":[{"platform":"ios","environment":"web"}],
        "remix":{"allowed":true,"license":"MIT"}}).to_string()).unwrap();
    let original = service
        .library
        .publish(PublishRequest {
            app_id: None,
            expected_revision: 0,
            source_path: source.to_string_lossy().into_owned(),
            authoring: None,
            market_origin: None,
        })
        .unwrap();
    let request = json!({"op":"remixAuthoringSource","requestId":uuid::Uuid::new_v4().to_string(),
        "appId":original.record.app_id,"revision":1,"name":"My copy"});
    let first = native(&service, request.clone()).await;
    assert_eq!(first["ok"], true, "{first}");
    let value = &first["value"];
    assert_ne!(value["record"]["appId"], original.record.app_id);
    assert_eq!(value["record"]["authoringSourceIds"][0], value["sourceId"]);
    assert_eq!(value["record"]["name"], "My copy");
    assert!(service.request_value(request.clone()).await.is_err());
    std::fs::remove_dir_all(value["sourcePath"].as_str().unwrap()).unwrap();
    assert_eq!(native(&service, request).await, first);
    assert_eq!(
        service
            .library
            .get(&original.record.app_id)
            .unwrap()
            .latest_revision,
        1
    );
}

#[tokio::test]
async fn native_market_remix_verifies_archive_and_replays_without_it() {
    let (_dir, service) = service();
    let (request, bytes) = market_remix_fixture(&service);
    let missing = native(&service, request.clone()).await;
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["error"]["code"], "archiveMissing");
    let path = Path::new(request["archivePath"].as_str().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    let first = native(&service, request.clone()).await;
    assert_eq!(first["ok"], true, "{first}");
    let value = &first["value"];
    assert_eq!(value["record"]["appId"], value["release"]["appId"]);
    assert_eq!(value["record"]["authoringSourceIds"][0], value["sourceId"]);
    assert_eq!(value["record"]["marketOrigin"]["remixedFrom"], "market-app");
    let imported = Path::new(value["sourcePath"].as_str().unwrap());
    assert_eq!(
        std::fs::read(imported.join(".gitignore")).unwrap(),
        b"build/\n"
    );
    assert_eq!(
        std::fs::read(imported.join(".well-known/security.txt")).unwrap(),
        b"Contact: security@example.test\n"
    );
    let session = uuid::Uuid::new_v4().to_string();
    let project = uuid::Uuid::new_v4().to_string();
    let registration = json!({"op":"registerAuthoring","requestId":request["requestId"],
        "sourceId":value["sourceId"],"sessionMode":"code","setCurrent":false,"authoring":{
            "sourceDirectory":value["sourceDirectory"],"workspaceId":uuid::Uuid::new_v4().to_string(),
            "projectRef":{"kind":"mobileProject","projectId":project},"authoringSessionId":session,
            "appId":value["record"]["appId"]}});
    let registered = native(&service, registration).await;
    assert_eq!(registered["ok"], true, "{registered}");
    assert_eq!(registered["value"]["sourceId"], value["sourceId"]);
    assert!(service
        .library
        .get(value["record"]["appId"].as_str().unwrap())
        .unwrap()
        .authoring
        .is_none());
    std::fs::remove_file(path).unwrap();
    assert_eq!(native(&service, request.clone()).await, first);
    let mut changed = request;
    changed["marketOrigin"]["releaseId"] = json!("other");
    assert_eq!(native(&service, changed).await["ok"], false);
}

#[tokio::test]
async fn native_market_remix_rejects_signed_zip_traversal_without_publication() {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let (dir, service) = service();
    let (mut request, original) = market_remix_fixture(&service);
    let mut input = zip::ZipArchive::new(std::io::Cursor::new(original)).unwrap();
    let mut output = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o600);
    for index in 0..input.len() {
        let mut entry = input.by_index(index).unwrap();
        let name = std::str::from_utf8(entry.name_raw()).unwrap().to_owned();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        output.start_file(name, options).unwrap();
        output.write_all(&content).unwrap();
    }
    output.start_file("../escape", options).unwrap();
    output.write_all(b"bad").unwrap();
    let archive = output.finish().unwrap().into_inner();
    let hash = format!("{:x}", Sha256::digest(&archive));
    request["archiveSha256"] = json!(hash);
    request["archiveBytes"] = json!(archive.len());
    request["marketOrigin"]["digest"] = json!(local_app::v2::market_release_digest(
        request["archiveSha256"].as_str().unwrap(),
        &request["manifest"],
        &request["files"],
        &request["dependencies"]
    )
    .unwrap());
    let path = Path::new(request["archivePath"].as_str().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, archive).unwrap();
    let rejected = native(&service, request).await;
    assert_eq!(rejected["ok"], false, "{rejected}");
    assert!(!dir.path().join("escape").exists());
    assert!(service.library.list().unwrap().is_empty());
}

#[tokio::test]
async fn trusted_authoring_registration_binds_first_publish_and_prepares_the_same_code_session() {
    let (_dir, service) = service();
    let creation_id = uuid::Uuid::new_v4().to_string();
    let created = native(
        &service,
        json!({"op":"createAuthoring","name":"First publish","requestId":creation_id}),
    )
    .await["value"]
        .clone();
    let retried = native(
        &service,
        json!({"op":"createAuthoring","name":"First publish","requestId":creation_id}),
    )
    .await["value"]
        .clone();
    assert_eq!(created, retried);
    assert_eq!(
        native(
            &service,
            json!({"op":"createAuthoring","name":"Different","requestId":creation_id})
        )
        .await["ok"],
        false
    );
    let source_directory = created["sourceDirectory"].as_str().unwrap().to_owned();
    let source_path = created["sourcePath"].as_str().unwrap().to_owned();
    let source_id = created["sourceId"].as_str().unwrap().to_owned();
    let prepare = json!({"op":"prepareAuthoringSource","sourceId":source_id,"sourceDirectory":source_directory});
    let proof: Value =
        serde_json::from_str(&service.clone().agent_host_json(prepare.to_string()).await).unwrap();
    assert_eq!(proof["ok"], true);
    assert_eq!(proof["value"]["sourceId"], source_id);
    let mut wrong_proof = prepare.clone();
    wrong_proof["sourceId"] = json!(uuid::Uuid::new_v4().to_string());
    let rejected: Value = serde_json::from_str(
        &service
            .clone()
            .agent_host_json(wrong_proof.to_string())
            .await,
    )
    .unwrap();
    assert_eq!(rejected["ok"], false);
    assert!(service.request_value(prepare).await.is_err());
    let workspace_id = uuid::Uuid::new_v4().to_string();
    let project_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let lookup = json!({"op":"getAuthoringGrant", "sessionMode":"code", "sourceDirectory":source_directory,
        "projectId":project_id, "authoringSessionId":session_id});
    let missing: Value =
        serde_json::from_str(&service.clone().agent_host_json(lookup.to_string()).await).unwrap();
    assert_eq!(missing["error"]["code"], "notFound");
    // The lookup is private to agentHostJson, not the ordinary app/page route.
    assert!(service.request_value(lookup.clone()).await.is_err());

    let incomplete = json!({
        "op":"registerAuthoring", "sourceId":source_id, "requestId":creation_id, "sessionMode":"code", "setCurrent":true,
        "authoring": {
            "sourceDirectory":source_directory,
            "workspaceId":workspace_id,
            "projectRef":{"kind":"mobileProject","projectId":project_id}
        }
    });
    let rejected: Value = serde_json::from_str(
        &service
            .clone()
            .agent_host_json(incomplete.to_string())
            .await,
    )
    .unwrap();
    assert_eq!(rejected["ok"], false);

    let outside = service
        .config_home
        .parent()
        .unwrap()
        .join(".atlas/.apps/foreign-source");
    std::fs::create_dir_all(&outside).unwrap();
    let outside_guest = service.guest_path(&outside).unwrap();
    let bad_source = json!({
        "op":"registerAuthoring", "sourceId":source_id, "requestId":creation_id, "sessionMode":"code", "setCurrent":true,
        "authoring": {
            "sourceDirectory":outside_guest,
            "workspaceId":workspace_id,
            "projectRef":{"kind":"mobileProject","projectId":project_id},
            "authoringSessionId":session_id
        }
    });
    let rejected: Value = serde_json::from_str(
        &service
            .clone()
            .agent_host_json(bad_source.to_string())
            .await,
    )
    .unwrap();
    assert_eq!(rejected["ok"], false);

    let rejected_publish = service
        .request_value(json!({
            "op":"publish",
            "sourcePath":source_path,
            "expectedRevision":0,
            "authoring":{
                "sourceDirectory":source_directory,
                "workspaceId":workspace_id,
                "projectRef":{"kind":"mobileProject","projectId":project_id},
                "authoringSessionId":session_id
            }
        }))
        .await
        .unwrap_err();
    assert_eq!(rejected_publish.code, "invalidRequest");

    let binding = json!({
        "sourceDirectory":source_directory,
        "workspaceId":workspace_id,
        "projectRef":{"kind":"mobileProject","projectId":project_id},
        "authoringSessionId":session_id
    });
    let registered: Value = serde_json::from_str(
        &service
            .clone()
            .agent_host_json(json!({"op":"registerAuthoring","sourceId":source_id,"requestId":creation_id,"sessionMode":"code","setCurrent":true,"authoring":binding}).to_string())
            .await,
    )
    .unwrap();
    assert_eq!(registered["ok"], true);
    assert_eq!(registered["value"]["sourceDirectory"], source_directory);
    assert_eq!(registered["value"]["workspaceId"], workspace_id);
    assert_eq!(registered["value"]["authoringSessionId"], session_id);
    assert!(registered["value"].get("appId").is_none());
    assert!(service.agent.lock().unwrap().is_none());
    let resolved: Value =
        serde_json::from_str(&service.clone().agent_host_json(lookup.to_string()).await).unwrap();
    assert_eq!(resolved["ok"], true);
    assert_eq!(resolved["value"]["workspaceId"], workspace_id);
    assert_eq!(resolved["value"]["projectRef"]["projectId"], project_id);
    let mut wrong_owner = lookup.clone();
    wrong_owner["authoringSessionId"] = json!(uuid::Uuid::new_v4().to_string());
    let wrong: Value = serde_json::from_str(
        &service
            .clone()
            .agent_host_json(wrong_owner.to_string())
            .await,
    )
    .unwrap();
    assert_eq!(wrong["error"]["code"], "notFound");

    let published = service
        .publish_from_trusted_source(
            &source_path,
            Some(&source_path),
            None,
            0,
            uuid::Uuid::new_v4().to_string(),
            session_id.clone(),
            Arc::new(Mutex::new(session_id.clone())),
        )
        .await
        .unwrap();
    let app_id = published["record"]["appId"].as_str().unwrap();
    assert_eq!(published["record"]["authoring"]["appId"], app_id);
    let resolved_published: Value =
        serde_json::from_str(&service.clone().agent_host_json(lookup.to_string()).await).unwrap();
    assert_eq!(resolved_published["ok"], true);
    assert!(resolved_published["value"].get("appId").is_none());
    assert_eq!(resolved_published["value"]["workspaceId"], workspace_id);
    assert_eq!(
        published["record"]["authoring"]["sourceDirectory"],
        source_directory
    );

    let mut session_bindings = serde_json::Map::new();
    session_bindings.insert(
        session_id.clone(),
        json!({
            "workspaceId":workspace_id,
            "sourceDirectory":source_directory,
            "projectRef":{"kind":"mobileProject","projectId":project_id},
            "mode":"code"
        }),
    );
    let prepared = service
        .request_value(json!({
            "op":"prepareAuthoring",
            "appId":app_id,
            "sessionBindings":session_bindings
        }))
        .await
        .unwrap();
    assert_eq!(prepared["action"], "resumeSession");
    assert_eq!(prepared["authoringSessionId"], session_id);
    assert_eq!(prepared["authoring"]["appId"], app_id);
    assert_eq!(prepared["sourceId"], source_id);
    let new_session = uuid::Uuid::new_v4().to_string();
    let rebound = json!({"sourceDirectory":source_directory,"workspaceId":workspace_id,
        "projectRef":{"kind":"mobileProject","projectId":project_id},
        "authoringSessionId":new_session,"appId":app_id});
    assert!(service
        .request_value(json!({"op":"bindAuthoring","appId":app_id,"authoring":rebound}))
        .await
        .is_err());
    let rebind_id = uuid::Uuid::new_v4().to_string();
    let binding_intent = native(&service, json!({"op":"bindAuthoringSource","requestId":rebind_id,"sourceId":source_id,"sourceDirectory":source_directory,"targetAppId":app_id})).await;
    assert_eq!(binding_intent["ok"], true);
    let registered_rebind: Value = serde_json::from_str(
        &service
            .clone()
            .agent_host_json(json!({"op":"registerAuthoring","sourceId":source_id,"requestId":rebind_id,"sessionMode":"code","setCurrent":true,"authoring":rebound}).to_string())
            .await,
    )
    .unwrap();
    assert_eq!(registered_rebind["ok"], true);
    let old: Value =
        serde_json::from_str(&service.clone().agent_host_json(lookup.to_string()).await).unwrap();
    assert_eq!(old["ok"], true);
    let new_lookup = json!({"op":"getAuthoringGrant","sessionMode":"code","sourceDirectory":source_directory,
        "projectId":project_id,"authoringSessionId":new_session});
    let new: Value = serde_json::from_str(
        &service
            .clone()
            .agent_host_json(new_lookup.to_string())
            .await,
    )
    .unwrap();
    assert_eq!(new["ok"], true);
}

#[tokio::test]
async fn native_restore_reuses_request_and_does_not_rebind_current_owner() {
    let (dir, service) = service();
    let published = publish(&service, dir.path(), "restore-source").await;
    let app_id = published["record"]["appId"].as_str().unwrap();
    let original = service.library.get(app_id).unwrap();
    let request_id = uuid::Uuid::new_v4().to_string();
    let request =
        json!({"op":"restoreAuthoringSource","requestId":request_id,"appId":app_id,"revision":1});
    let restored = native(&service, request.clone()).await;
    assert_eq!(restored["ok"], true, "{restored}");
    let retry = native(&service, request.clone()).await;
    assert_eq!(restored, retry);
    assert_eq!(
        service.library.get(app_id).unwrap().authoring,
        original.authoring
    );
    let value = &restored["value"];
    let proof = native(&service, json!({"op":"prepareAuthoringSource","sourceId":value["sourceId"],"sourceDirectory":value["sourceDirectory"]})).await;
    assert_eq!(proof["ok"], true);
    let binding = json!({"sourceDirectory":value["sourceDirectory"],"workspaceId":uuid::Uuid::new_v4().to_string(),
        "projectRef":{"kind":"mobileProject","projectId":uuid::Uuid::new_v4().to_string()},
        "authoringSessionId":uuid::Uuid::new_v4().to_string(),"appId":app_id});
    let admitted = native(
        &service,
        json!({"op":"registerAuthoring","sourceId":value["sourceId"],"requestId":request_id,
        "sessionMode":"code","setCurrent":false,"authoring":binding}),
    )
    .await;
    assert_eq!(admitted["ok"], true, "{admitted}");
    assert_eq!(
        service.library.get(app_id).unwrap().authoring,
        original.authoring
    );
    let conflict = native(
        &service,
        json!({"op":"restoreAuthoringSource","requestId":request_id,"appId":app_id,"revision":2}),
    )
    .await;
    assert_eq!(conflict["ok"], false);
    assert!(service.request_value(request).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_authoring_grants_preserve_rebound_owner_and_reject_replaced_source() {
    use std::os::unix::fs::symlink;
    let (_dir, service) = service();
    let creation_id = uuid::Uuid::new_v4().to_string();
    let created = native(
        &service,
        json!({"op":"createAuthoring","name":"Owner","requestId":creation_id}),
    )
    .await["value"]
        .clone();
    let source = created["sourceDirectory"].as_str().unwrap();
    let source_id = created["sourceId"].as_str().unwrap();
    let source_path = PathBuf::from(created["sourcePath"].as_str().unwrap());
    let first_project = uuid::Uuid::new_v4().to_string();
    let first_session = uuid::Uuid::new_v4().to_string();
    let new_project = uuid::Uuid::new_v4().to_string();
    let new_session = uuid::Uuid::new_v4().to_string();
    let workspace = uuid::Uuid::new_v4().to_string();
    for (project, session) in [
        (&first_project, &first_session),
        (&new_project, &new_session),
    ] {
        let request = json!({"op":"registerAuthoring", "sourceId":source_id,"requestId":creation_id,"sessionMode":"code","setCurrent":true,"authoring":{
            "sourceDirectory":source,"workspaceId":workspace,
            "projectRef":{"kind":"mobileProject","projectId":project},"authoringSessionId":session}});
        let result: Value =
            serde_json::from_str(&service.clone().agent_host_json(request.to_string()).await)
                .unwrap();
        assert_eq!(result["ok"], true);
    }
    let old = json!({"op":"getAuthoringGrant","sessionMode":"code","sourceDirectory":source,"projectId":first_project,"authoringSessionId":first_session});
    let old_result: Value =
        serde_json::from_str(&service.clone().agent_host_json(old.to_string()).await).unwrap();
    assert_eq!(old_result["ok"], true);
    let new = json!({"op":"getAuthoringGrant","sessionMode":"code","sourceDirectory":source,"projectId":new_project,"authoringSessionId":new_session});
    let saved = source_path.with_extension("saved");
    std::fs::rename(&source_path, &saved).unwrap();
    std::fs::create_dir(&source_path).unwrap();
    let rejected_publish = service
        .publish_from_trusted_source(
            source_path.to_str().unwrap(),
            Some(source_path.to_str().unwrap()),
            None,
            0,
            uuid::Uuid::new_v4().to_string(),
            new_session.clone(),
            Arc::new(Mutex::new(new_session.clone())),
        )
        .await;
    assert!(rejected_publish.is_err());
    assert!(service.library.list().unwrap().is_empty());
    std::fs::remove_dir(&source_path).unwrap();
    symlink(&saved, &source_path).unwrap();
    let rejected: Value =
        serde_json::from_str(&service.clone().agent_host_json(new.to_string()).await).unwrap();
    assert_eq!(rejected["ok"], false);
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

async fn loopback_get(launch: &Value) -> String {
    let origin = launch["origin"].as_str().unwrap();
    let url = launch["url"].as_str().unwrap();
    let path = url.strip_prefix(origin).unwrap();
    let port = origin.rsplit(':').next().unwrap().parse::<u16>().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), get(port, path))
        .await
        .expect("loopback gateway did not answer within 3 seconds")
}

async fn wait_for_gateway_cleanup(service: &Arc<MobileLocalAppServiceHandle>, app_id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if !service.active.lock().await.contains_key(app_id) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed gateway run should be cleaned up");
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
async fn external_browser_static_html_loads_the_run_scoped_webmcp_resource_first() {
    let (dir, service) = service();
    let source = dir.path().join("webmcp-static");
    std::fs::create_dir_all(source.join("dist")).unwrap();
    std::fs::write(
        source.join("dist/index.html"),
        "<!doctype html><html><head><title>Local App</title><script id=\"app-start\"></script></head><body></body></html>",
    )
    .unwrap();
    std::fs::write(
        source.join("lingxi-app.json"),
        json!({
            "schema":2,
            "name":"WebMCP static app",
            "entry":{"type":"static","path":"dist/index.html"},
            "targets":[{"platform":"ios","environment":"web"}]
        })
        .to_string(),
    )
    .unwrap();
    let published = service
        .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":source}))
        .await
        .unwrap();
    let id = published["record"]["appId"].as_str().unwrap();
    let port = published["record"]["originPort"].as_u64().unwrap() as u16;
    let launch = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap();
    let run_id = launch["run"]["runId"].as_str().unwrap();

    let page = get(port, &format!("/__run/{run_id}/dist/index.html")).await;
    assert!(page.starts_with("HTTP/1.1 200 OK"));
    assert!(page.contains("Content-Security-Policy:"));
    assert!(page.contains("script-src 'self'"));
    let html = page.split_once("\r\n\r\n").unwrap().1;
    let script =
        format!("<script src=\"/__run/{run_id}/__host/webmcp.js\" data-lingxi-webmcp></script>");
    assert_eq!(html.matches("data-lingxi-webmcp").count(), 1);
    assert!(html.find(&script).unwrap() < html.find("id=\"app-start\"").unwrap());

    let resource = get(port, &format!("/__run/{run_id}/__host/webmcp.js")).await;
    assert!(resource.starts_with("HTTP/1.1 200 OK"));
    assert!(resource.contains("Content-Type: text/javascript; charset=utf-8"));
    assert!(resource.contains("SPDX-License-Identifier: Apache-2.0"));
    assert!(resource.contains("document.modelContext"));

    let stale = get(port, "/__run/old-run/__host/webmcp.js").await;
    assert!(stale.starts_with("HTTP/1.1 410"));
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
async fn gateway_accept_service_retries_connection_aborted_and_serves_the_app() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (dir, service) = service();
    let published = publish(&service, dir.path(), "accept-retry").await;
    let app_id = published["record"]["appId"].as_str().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempt_counter = attempts.clone();
    let acceptor: gateway::AcceptFactory = Arc::new(move |listener| {
        let attempt = attempt_counter.fetch_add(1, Ordering::SeqCst);
        let future: gateway::AcceptFuture = if attempt == 0 {
            Box::pin(async { Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted)) })
        } else {
            Box::pin(async move { listener.accept().await })
        };
        future
    });
    *service.gateway_acceptor.lock().unwrap() = Some(acceptor);

    let launch = service
        .request_value(json!({"op":"start","appId":app_id}))
        .await
        .unwrap();
    let response = loopback_get(&launch).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("hello independent app"));
    assert!(attempts.load(Ordering::SeqCst) >= 2);

    service
        .request_value(json!({"op":"stop","appId":app_id}))
        .await
        .unwrap();
}

#[tokio::test]
async fn gateway_accept_service_cleans_fatal_exit_and_next_start_serves() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (dir, service) = service();
    let published = publish(&service, dir.path(), "accept-fatal").await;
    let app_id = published["record"]["appId"].as_str().unwrap();
    let first_failure = Arc::new(AtomicBool::new(true));
    let entered_failure = Arc::new(tokio::sync::Notify::new());
    let release_failure = Arc::new(tokio::sync::Notify::new());
    let fail_once = first_failure.clone();
    let entered = entered_failure.clone();
    let release = release_failure.clone();
    let acceptor: gateway::AcceptFactory = Arc::new(move |listener| {
        let future: gateway::AcceptFuture = if fail_once.swap(false, Ordering::SeqCst) {
            let entered = entered.clone();
            let release = release.clone();
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            })
        } else {
            Box::pin(async move { listener.accept().await })
        };
        future
    });
    *service.gateway_acceptor.lock().unwrap() = Some(acceptor);

    let first = service
        .request_value(json!({"op":"start","appId":app_id}))
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        entered_failure.notified(),
    )
    .await
    .expect("injected terminal accept error should be reached");
    let failed_run = service
        .active
        .lock()
        .await
        .get(app_id)
        .cloned()
        .expect("started run should remain registered before releasing the error");
    release_failure.notify_one();
    wait_for_gateway_cleanup(&service, app_id).await;
    assert_eq!(failed_run.record.lock().unwrap().phase, RunPhase::Failed);
    assert!(failed_run.url.lock().unwrap().is_empty());
    assert!(failed_run
        .record
        .lock()
        .unwrap()
        .error
        .as_deref()
        .is_some_and(|error| error.contains("PermissionDenied")));

    let second = service
        .request_value(json!({"op":"start","appId":app_id}))
        .await
        .unwrap();
    assert_ne!(first["run"]["runId"], second["run"]["runId"]);
    let response = loopback_get(&second).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("hello independent app"));
    service
        .request_value(json!({"op":"stop","appId":app_id}))
        .await
        .unwrap();
}

#[tokio::test]
async fn gateway_accept_failure_before_start_response_returns_error_and_recovers() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (dir, service) = service();
    let published = publish(&service, dir.path(), "accept-start-fatal").await;
    let app_id = published["record"]["appId"].as_str().unwrap().to_owned();
    let previous = service
        .request_value(json!({"op":"start","appId":app_id}))
        .await
        .unwrap();
    let previous_run = service
        .active
        .lock()
        .await
        .get(&app_id)
        .cloned()
        .expect("the initial run should be registered");
    let source = dir.path().join("accept-start-fatal");
    std::fs::write(source.join("index.html"), "updated independent app").unwrap();
    service
        .request_value(json!({
            "op":"publish",
            "appId":app_id,
            "expectedRevision":1,
            "sourcePath":source
        }))
        .await
        .unwrap();
    let fail_once = Arc::new(AtomicBool::new(true));
    let should_fail = fail_once.clone();
    let acceptor: gateway::AcceptFactory = Arc::new(move |listener| {
        let future: gateway::AcceptFuture = if should_fail.swap(false, Ordering::SeqCst) {
            Box::pin(async { Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)) })
        } else {
            Box::pin(async move { listener.accept().await })
        };
        future
    });
    *service.gateway_acceptor.lock().unwrap() = Some(acceptor);

    let gate = GatewayStartGate {
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let gate_entered = gate.entered.clone();
    let gate_release = gate.release.clone();
    *service.gateway_start_gate.lock().unwrap() = Some(gate);

    let starting_service = service.clone();
    let starting_app_id = app_id.clone();
    let starting = tokio::spawn(async move {
        starting_service
            .request_value(json!({
                "op":"start",
                "appId":starting_app_id,
                "revision":2
            }))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), gate_entered.notified())
        .await
        .expect("startup should reach the gateway response gate");

    let failed_run = service
        .active
        .lock()
        .await
        .get(&app_id)
        .cloned()
        .expect("starting run should be registered");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let is_failed = { failed_run.gateway_failure.lock().unwrap().is_some() };
            if is_failed {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fatal accept should be marked before startup returns");

    gate_release.notify_one();
    let error = starting.await.unwrap().unwrap_err();
    assert_eq!(error.code, "gatewayFailed");
    assert_eq!(failed_run.record.lock().unwrap().phase, RunPhase::Failed);
    assert!(failed_run.url.lock().unwrap().is_empty());
    assert!(previous_run.cancel.is_cancelled());
    assert!(!service.active.lock().await.contains_key(&app_id));

    let second = service
        .request_value(json!({"op":"start","appId":app_id,"revision":2}))
        .await
        .unwrap();
    assert_ne!(previous["run"]["runId"], second["run"]["runId"]);
    let response = loopback_get(&second).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("updated independent app"));
    service
        .request_value(json!({"op":"stop","appId":published["record"]["appId"]}))
        .await
        .unwrap();
}

#[tokio::test]
async fn gateway_accept_abort_is_recovered_by_next_start_without_callback() {
    let (dir, service) = service();
    let published = publish(&service, dir.path(), "accept-aborted").await;
    let app_id = published["record"]["appId"].as_str().unwrap();
    let first = service
        .request_value(json!({"op":"start","appId":app_id}))
        .await
        .unwrap();
    let failed_run = service.active.lock().await.get(app_id).cloned().unwrap();
    {
        let gateway = failed_run.gateway.lock().await;
        gateway.as_ref().unwrap().abort();
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if failed_run
                .gateway
                .lock()
                .await
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted gateway task should finish");

    // Aborting a task drops its future and does not execute the accept-error
    // callback. Same-version start must notice the finished JoinHandle, clean
    // the old run, and start a new gateway.
    let second = service
        .request_value(json!({"op":"start","appId":app_id}))
        .await
        .unwrap();
    assert_ne!(first["run"]["runId"], second["run"]["runId"]);
    assert_eq!(failed_run.record.lock().unwrap().phase, RunPhase::Failed);
    assert!(failed_run.url.lock().unwrap().is_empty());
    let response = loopback_get(&second).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("hello independent app"));
    service
        .request_value(json!({"op":"stop","appId":app_id}))
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
    use base64::Engine as _;

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
    assert!(sdk.contains(&format!(
        "\"productName\":{}",
        serde_json::to_string(branding::PRODUCT_NAME).unwrap()
    )));
    assert!(!sdk.contains("__LINGXI_APP_CONFIGURATION__"));
    assert!(sdk.contains("window.LingxiLocalAppAGUI"));
    assert!(sdk.contains("installBrowserAgent"));
    assert!(sdk.contains("agent.tasks.events"));
    assert!(sdk.contains("application/json"));
    assert!(sdk.contains("RUN_FINISHED"));

    let recovery = get(port, &format!("/__run/{epoch}/__host/recovery-sw.js")).await;
    assert!(recovery.starts_with("HTTP/1.1 200 OK"));
    assert!(recovery.contains("Service-Worker-Allowed: /"));
    assert!(recovery.contains(&format!("\"appId\":\"{id}\"")));
    assert!(recovery.contains(&format!(
        "\"productName\":{}",
        serde_json::to_string(branding::PRODUCT_NAME).unwrap()
    )));
    assert!(recovery.contains("runId"));
    assert!(recovery.contains("lingxi-local-app-agui-"));
    assert!(recovery.contains("record.runId"));
    assert!(recovery.contains("id.textContent=record.runId"));
    assert!(!recovery.contains("cache.put(request"));
    assert!(
        !recovery.contains("caches.match(request)"),
        "the worker must not cache arbitrary pages or Agent responses"
    );

    fn input(run_id: &str) -> Value {
        json!({
            "threadId": format!("thread-{run_id}"),
            "runId": run_id,
            "protocolVersion": "1.0",
            "messages": [{
                "id": format!("message-{run_id}"),
                "role": "user",
                "content": [{"type":"text","text":"test"}]
            }],
            "forwardedProps": {"com.lingxi.localApp":{"version":1}}
        })
    }

    async fn post_body(
        port: u16,
        epoch: &str,
        origin: bool,
        referer_epoch: &str,
        content_type: &str,
        body: &str,
    ) -> String {
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let origin = if origin {
            format!("Origin: http://127.0.0.1:{port}\r\n")
        } else {
            String::new()
        };
        socket.write_all(format!("POST /__run/{epoch}/__host/agent HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{origin}Referer: http://127.0.0.1:{port}/__run/{referer_epoch}/index.html\r\nContent-Type: {content_type}\r\nX-LingXi-App-Request: 1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        let mut out = Vec::new();
        socket.read_to_end(&mut out).await.unwrap();
        String::from_utf8(out).unwrap()
    }

    async fn post(
        port: u16,
        epoch: &str,
        origin: bool,
        referer_epoch: &str,
        method: &str,
        args: Value,
        extra: &str,
    ) -> String {
        let mut body = json!({"method":method,"args":args}).to_string();
        if !extra.is_empty() {
            body.pop();
            body.push_str(extra);
            body.push('}');
        }
        post_body(
            port,
            epoch,
            origin,
            referer_epoch,
            "application/json",
            &body,
        )
        .await
    }

    async fn framing_response(port: u16, epoch: &str, framing: &str) -> String {
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        socket.write_all(format!("POST /__run/{epoch}/__host/agent HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nReferer: http://127.0.0.1:{port}/__run/{epoch}/index.html\r\nContent-Type: application/json\r\nX-LingXi-App-Request: 1\r\n{framing}\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = Vec::new();
        socket.read_to_end(&mut out).await.unwrap();
        String::from_utf8(out).unwrap()
    }

    assert!(
        framing_response(port, epoch, "Transfer-Encoding: chunked\r\n")
            .await
            .starts_with("HTTP/1.1 400")
    );
    assert!(framing_response(
        port,
        epoch,
        &format!("Content-Length: {}\r\n", 116 * 1024 * 1024 + 1)
    )
    .await
    .starts_with("HTTP/1.1 413"));
    assert!(post(
        port,
        epoch,
        false,
        epoch,
        "agent.tasks.start",
        json!({"input": input("gateway-test-run")}),
        ""
    )
    .await
    .starts_with("HTTP/1.1 403"));
    assert!(post_body(
        port,
        epoch,
        true,
        epoch,
        "multipart/form-data; boundary=legacy",
        "{}"
    )
    .await
    .starts_with("HTTP/1.1 400"));
    assert!(post(
        port,
        epoch,
        true,
        "stale",
        "agent.tasks.start",
        json!({"input": input("gateway-test-run")}),
        ""
    )
    .await
    .starts_with("HTTP/1.1 403"));
    assert!(post(port, epoch, true, epoch, "answer", json!({}), "")
        .await
        .starts_with("HTTP/1.1 403"));
    assert!(post(
        port,
        epoch,
        true,
        epoch,
        "eventsNative",
        json!({"runId":"private-run"}),
        ""
    )
    .await
    .starts_with("HTTP/1.1 403"));
    assert!(post(
        port,
        epoch,
        true,
        epoch,
        "agent.tasks.start",
        json!({"input": input("gateway-test-run")}),
        ",\"appId\":\"other\""
    )
    .await
    .starts_with("HTTP/1.1 400"));
    assert!(post(
        port,
        epoch,
        true,
        epoch,
        "agent.tasks.events",
        json!({"runId":"missing-run","afterSequence":0}),
        ""
    )
    .await
    .starts_with("HTTP/1.1 200"));

    let bytes = vec![0x5a; 1024 * 1024 + 17];
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let large_input = json!({
        "threadId":"thread-large-file",
        "runId":"gateway-large-file-run",
        "protocolVersion":"1.0",
        "messages":[{
            "id":"message-large-file",
            "role":"user",
            "content":[
                {"type":"text","text":"Inspect this document"},
                {
                    "type":"document",
                    "source":{"type":"data","value":encoded,"mimeType":"application/octet-stream"},
                    "metadata":{"com.lingxi.localApp":{"version":1,"filename":"large.bin"}}
                }
            ]
        }],
        "forwardedProps":{"com.lingxi.localApp":{"version":1}}
    });
    let large_body = json!({
        "method":"agent.tasks.start",
        "args":{"input":large_input}
    })
    .to_string();
    assert!(large_body.len() > 1024 * 1024);
    let no_model = post_body(port, epoch, true, epoch, "application/json", &large_body).await;
    assert!(no_model.starts_with("HTTP/1.1 200"));
    assert!(no_model.contains("agent_model_host_not_ready"));

    service
        .request_value(json!({"op":"restart","appId":id}))
        .await
        .unwrap();
    assert!(post(
        port,
        epoch,
        true,
        epoch,
        "agent.tasks.start",
        json!({"input": input("gateway-after-restart")}),
        ""
    )
    .await
    .starts_with("HTTP/1.1 410"));
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}
