//! Model-facing project adapter. Administrative/consent operations are never tools.
use super::unified_local_apps::MobileLocalAppServiceHandle;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tool_ui::desktop_local_app::ProjectLocalAppHost;

pub(crate) struct MobileProjectAppHost {
    pub service: Arc<MobileLocalAppServiceHandle>,
    pub config_home: PathBuf,
    /// Live engine-owned UUID, never a model-provided field.
    pub active_session_uuid: Arc<std::sync::Mutex<String>>,
    /// Metadata only; this factory is never bound as the application's executor.
    pub catalog: Arc<std::sync::OnceLock<std::sync::Weak<super::app_agent::AppAgentFactory>>>,
}

/// Inline HTML is already captured in the tool invocation. Persist its exact
/// request before materializing files, so a restarted invocation can recreate
/// missing atomic writes without reading a mutable external source.
fn stage_inline_publication(
    config_home: &Path,
    request_id: &str,
    input: &Value,
    html: &str,
    manifest: &Value,
) -> Result<(PathBuf, rooted_fs::RootedFileLock), String> {
    let imports = config_home.join("local-app-imports");
    let imports_pin =
        rooted_fs::ensure_private_directory(config_home, Path::new("local-app-imports"), 0o700)
            .map_err(|e| e.to_string())?;
    let lock_name = format!(".inline-{request_id}.lock");
    let lock = rooted_fs::lock_exclusive(&imports, Path::new(&lock_name), 0o700, 0o600)
        .map_err(|e| e.to_string())?;
    if rooted_fs::root_identity(&imports).map_err(|e| e.to_string())? != imports_pin {
        return Err("inline staging root changed".into());
    }
    let name = format!("inline-{request_id}");
    let path = imports.join(&name);
    let created_identity = match rooted_fs::create_directory_new_pinned(
        &imports,
        Path::new(&name),
        0o700,
        &imports_pin,
    ) {
        Ok(opened) => Some(rooted_fs::opened_file_identity(&opened).map_err(|e| e.to_string())?),
        Err(_) => None,
    };
    let pin = rooted_fs::ensure_directory_identity_pinned(&imports, Path::new(&name), &imports_pin)
        .map_err(|e| e.to_string())?;
    if created_identity
        .as_ref()
        .is_some_and(|created| created != &pin)
    {
        return Err("inline staging directory changed".into());
    }
    let options = rooted_fs::AtomicWriteOptions {
        overwrite: false,
        create_parents: false,
        ..Default::default()
    };
    let intent = json!({"requestId":request_id,"request":input});
    let intent_path = Path::new("intent.json");
    if created_identity.is_some() {
        rooted_fs::atomic_write_pinned(
            &path,
            intent_path,
            &serde_json::to_vec(&intent).map_err(|e| e.to_string())?,
            options,
            &pin,
        )
        .map_err(|e| e.to_string())?;
    } else {
        let saved = rooted_fs::read_to_string_pinned(&path, intent_path, Some(&pin))
            .map_err(|_| "inline staging outcome is unknown".to_string())?;
        let saved: Value = serde_json::from_str(&saved)
            .map_err(|_| "inline staging intent is invalid".to_string())?;
        if saved != intent {
            return Err("publication request id is bound to different input".into());
        }
    }
    let manifest_text = manifest.to_string();
    for (name, bytes) in [
        ("index.html", html.as_bytes()),
        ("lingxi-app.json", manifest_text.as_bytes()),
    ] {
        let relative = Path::new(name);
        match rooted_fs::read_to_string_pinned(&path, relative, Some(&pin)) {
            Ok(existing) if existing.as_bytes() == bytes => {}
            Ok(_) => return Err("inline staging content changed".into()),
            Err(lingxi_core::host::FsError::NotFound(_)) => {
                rooted_fs::atomic_write_pinned(&path, relative, bytes, options, &pin)
                    .map_err(|e| e.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok((path, lock))
}

impl MobileProjectAppHost {
    async fn dispatch(&self, request: Value) -> Result<Value, String> {
        self.service
            .clone()
            .request_for_tool(request)
            .await
            .map_err(|error| format!("{}: {}", error.code, error.message))
    }
}

fn installed_app_metadata(record: &Value) -> Result<Value, String> {
    let object = record.as_object().ok_or("invalid application record")?;
    let mut result = serde_json::Map::new();
    for key in [
        "appId",
        "name",
        "summary",
        "currentRevision",
        "latestRevision",
        "marketOrigin",
    ] {
        if let Some(value) = object.get(key) {
            result.insert(key.to_owned(), value.clone());
        }
    }
    Ok(Value::Object(result))
}

fn installed_runtime_metadata(value: Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or("invalid application runtime status")?;
    let mut result = serde_json::Map::new();
    for key in ["url", "origin", "stopped"] {
        if let Some(value) = object.get(key) {
            result.insert(key.to_owned(), value.clone());
        }
    }
    for key in ["run", "preparingRun"] {
        if let Some(run) = object.get(key) {
            if run.is_null() {
                result.insert(key.to_owned(), Value::Null);
                continue;
            }
            let run = run.as_object().ok_or("invalid application run")?;
            let mut projected = serde_json::Map::new();
            for field in [
                "appId",
                "runId",
                "revision",
                "releaseDigest",
                "phase",
                "services",
                "startedAtMs",
                "updatedAtMs",
            ] {
                if let Some(value) = run.get(field) {
                    projected.insert(field.to_owned(), value.clone());
                }
            }
            result.insert(key.to_owned(), Value::Object(projected));
        }
    }
    Ok(Value::Object(result))
}

#[async_trait]
impl ProjectLocalAppHost for MobileProjectAppHost {
    async fn request(&self, input: Value) -> Result<Value, String> {
        let app_id = input["id"].as_str();
        let action = input["action"].as_str().unwrap_or("publish");
        if matches!(action, "publish" | "read")
            && (input["source_session"]["mode"].as_str() != Some("code")
                || input["source_session"]["sessionId"]
                    .as_str()
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
                    .is_none())
        {
            return Err("local_app_development_requires_code: Local App development requires a Code session. Switch to Code to continue.".into());
        }
        match action {
            "agent_catalog" => {
                let factory = self.catalog.get().and_then(std::sync::Weak::upgrade).ok_or("Host tool catalog is not ready")?;
                let catalog = factory.catalog().await?;
                let tools = catalog["tools"].as_object().ok_or("Invalid host tool catalog")?
                    .iter().map(|(id, grant)| {
                        let mut value = grant.clone();
                        value["id"] = json!(id);
                        value["label"] = catalog["labels"][id].as_str().map(|label| json!(label)).unwrap_or_else(|| json!(id));
                        value
                    }).collect::<Vec<_>>();
                Ok(json!({"models":catalog["models"],"tools":tools}))
            }
            "list" => {
                let records = self.dispatch(json!({"op":"list"})).await?;
                let records = records.as_array().ok_or("invalid application list")?;
                Ok(Value::Array(records.iter().map(installed_app_metadata).collect::<Result<Vec<_>, _>>()?))
            }
            "read" => {
                let app_id = app_id.ok_or("Application id is required")?;
                let record = self.dispatch(json!({"op":"get","appId":app_id})).await?;
                let release = self.dispatch(json!({"op":"getRelease","appId":app_id,"revision":record["latestRevision"]})).await?;
                Ok(json!({"record":record,"release":release}))
            }
            "logs" => {
                let app_id = app_id.ok_or("Application id is required")?;
                let runs = self.dispatch(json!({"op":"runs","appId":app_id})).await?;
                let run = runs.as_array().and_then(|runs| runs.last());
                match run.and_then(|run| run["runId"].as_str()) {
                    Some(run_id) => {
                        let text = self.dispatch(json!({"op":"logs","appId":app_id,"runId":run_id})).await?;
                        Ok(json!({"appId":app_id,"runId":run_id,"text":text}))
                    }
                    None => Ok(json!({"appId":app_id,"runId":null,"text":""})),
                }
            }
            "start" | "stop" | "status" => {
                installed_runtime_metadata(self.dispatch(json!({"op":input["action"],"appId":app_id.ok_or("Application id is required")?})).await?)
            }
            "publish" => {
                let engine_session = input["source_session"]["sessionId"].as_str().unwrap_or("").to_owned();
                {
                    let live = self.active_session_uuid.lock().map_err(|_| "engine owner unavailable")?;
                    if *live != engine_session { return Err("engine session changed during publication".into()); }
                }
                let request_id = input["source_session"]["requestId"].as_str()
                    .filter(|id| uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == *id))
                    .ok_or("local_app_origin_unavailable: durable publication identity is required")?.to_owned();
                let mut inline_lock = None;
                let source = if let Some(staged) = input["staged_path"].as_str() { staged.to_owned() }
                else if let Some(html) = input["html"].as_str() {
                    local_app::v2::validate_inline_document(html)?;
                    let name = input["name"].as_str().unwrap_or("Local App");
                    let manifest = json!({"schema":2,"name":name,"summary":input["summary"].as_str().unwrap_or(""),"entry":{"type":"static","path":"index.html"},"targets":[{"platform":"android","environment":"web"},{"platform":"ios","environment":"web"},{"platform":"macos","environment":"web"},{"platform":"windows","environment":"web"},{"platform":"linux","environment":"web"},{"platform":"web","environment":"web"}],"package":{"files":["index.html"],"dependencyLocks":[]}});
                    let config_home = self.config_home.clone();
                    let stage_id = request_id.clone();
                    let saved_input = input.clone();
                    let html = html.to_owned();
                    let (path, lock) = tokio::task::spawn_blocking(move || stage_inline_publication(&config_home, &stage_id, &saved_input, &html, &manifest))
                        .await.map_err(|error| error.to_string())??;
                    inline_lock = Some(lock);
                    path.to_string_lossy().into_owned()
                } else { return Err("A staged project or HTML document is required".into()); };
                let result = self
                    .service
                    .clone()
                    .publish_from_trusted_source(
                        &source,
                        input["source_path"].as_str(),
                        app_id,
                        input["expectedRevision"].as_u64().unwrap_or(0),
                        request_id,
                        engine_session,
                        self.active_session_uuid.clone(),
                    )
                    .await
                    .map_err(|error| format!("{}: {}", error.code, error.message))?;
                drop(inline_lock);
                Ok(json!({"action":"publish","id":result["record"]["appId"],"name":result["record"]["name"],"summary":result["record"]["summary"],"revision":result["release"]["revision"],"record":result["record"],"release":result["release"]}))
            }
            _ => Err("Unknown Local App action; consent and administration are native-only".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tool_ui::desktop_local_app::ProjectLocalAppHost;

    #[tokio::test]
    async fn inline_publication_uses_only_the_current_schema_two_library() {
        let root = tempfile::tempdir().unwrap();
        let service =
            // The config home's parent is the guest Linux user directory,
            // whose name must be a plain identifier (not `.tmpXXXX`).
            MobileLocalAppServiceHandle::new(
                root.path().join("user").join("config"),
                None,
                "android".into(),
            )
            .unwrap();
        let host = MobileProjectAppHost {
            service: service.clone(),
            config_home: service.config_home.clone(),
            active_session_uuid: Arc::new(std::sync::Mutex::new(uuid::Uuid::new_v4().to_string())),
            catalog: Arc::new(std::sync::OnceLock::new()),
        };
        let html = "<!doctype html><html><body>Current static source</body></html>";
        let session = host.active_session_uuid.lock().unwrap().clone();
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = json!({
            "action":"publish", "name":"Current source", "html":html,
                "source_session":{"sessionId":session,"mode":"code","requestId":request_id.clone()}
        });
        let result = host.request(request.clone()).await.unwrap();
        let app_id = result["id"].as_str().unwrap();
        assert_eq!(result["release"]["manifest"]["schema"], 2);
        assert_eq!(service.library.list().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(
                service
                    .library
                    .source_path(app_id, 1)
                    .unwrap()
                    .join("index.html")
            )
            .unwrap(),
            html
        );
        assert!(!service.config_home.join("local-apps/index.json").exists());
        let retained = service
            .config_home
            .join("local-app-imports")
            .join(format!("inline-{request_id}"));
        assert_eq!(
            std::fs::read_to_string(retained.join("index.html")).unwrap(),
            html
        );
        let replay = host.request(request.clone()).await.unwrap();
        assert_eq!(replay["id"], result["id"]);
        assert_eq!(replay["revision"], result["revision"]);
        let mut changed = request;
        changed["html"] = json!("<!doctype html><html><body>changed</body></html>");
        assert!(host.request(changed).await.is_err());
        assert_eq!(
            std::fs::read_to_string(retained.join("index.html")).unwrap(),
            html
        );
        assert_eq!(service.library.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invalid_inline_document_fails_before_creating_any_staged_source() {
        let root = tempfile::tempdir().unwrap();
        let service =
            // The config home's parent is the guest Linux user directory,
            // whose name must be a plain identifier (not `.tmpXXXX`).
            MobileLocalAppServiceHandle::new(
                root.path().join("user").join("config"),
                None,
                "android".into(),
            )
            .unwrap();
        let host = MobileProjectAppHost {
            service: service.clone(),
            config_home: service.config_home.clone(),
            active_session_uuid: Arc::new(std::sync::Mutex::new(uuid::Uuid::new_v4().to_string())),
            catalog: Arc::new(std::sync::OnceLock::new()),
        };
        let session = host.active_session_uuid.lock().unwrap().clone();
        assert!(host
            .request(json!({
                "action":"publish", "name":"Fragment", "html":"<div>fragment</div>",
                "source_session":{"sessionId":session,"mode":"code","requestId":uuid::Uuid::new_v4().to_string()}
            }))
            .await
            .is_err());
        assert!(service.library.list().unwrap().is_empty());
        assert!(!service.config_home.join("local-app-imports").exists());
        assert!(!service.config_home.join("local-apps/index.json").exists());
    }

    #[tokio::test]
    async fn changed_engine_owner_cannot_stage_or_publish_inline_source() {
        let root = tempfile::tempdir().unwrap();
        let service =
            // The config home's parent is the guest Linux user directory,
            // whose name must be a plain identifier (not `.tmpXXXX`).
            MobileLocalAppServiceHandle::new(
                root.path().join("user").join("config"),
                None,
                "android".into(),
            )
            .unwrap();
        let host = MobileProjectAppHost {
            service: service.clone(),
            config_home: service.config_home.clone(),
            active_session_uuid: Arc::new(std::sync::Mutex::new(uuid::Uuid::new_v4().to_string())),
            catalog: Arc::new(std::sync::OnceLock::new()),
        };
        let old_owner = uuid::Uuid::new_v4().to_string();
        let error = host.request(json!({
            "action":"publish", "html":"<!doctype html><html><body>old</body></html>",
            "source_session":{"sessionId":old_owner,"mode":"code","requestId":uuid::Uuid::new_v4().to_string()}
        })).await.unwrap_err();
        assert!(error.contains("engine session changed"));
        assert!(!service.config_home.join("local-app-imports").exists());
        assert!(service.library.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn staged_publish_uses_trusted_original_source_for_pending_authoring_lookup() {
        let root = tempfile::tempdir().unwrap();
        let config_home = root.path().join("persistent/mobile-user/.atlas");
        let service =
            MobileLocalAppServiceHandle::new(config_home.clone(), None, "android".into()).unwrap();
        let config_home = service.config_home.clone();
        let creation_id = uuid::Uuid::new_v4().to_string();
        let response: Value = serde_json::from_str(
            &service
                .clone()
                .agent_host_json(
                    json!({"op":"createAuthoring","name":"First app","requestId":creation_id})
                        .to_string(),
                )
                .await,
        )
        .unwrap();
        assert_eq!(response["ok"], true);
        let created = response["value"].clone();
        let original_source = PathBuf::from(created["sourcePath"].as_str().unwrap());
        let source_id = created["sourceId"].as_str().unwrap();
        std::fs::write(
            original_source.join("lingxi-app.json"),
            r#"{"schema":2,"name":"Staged app","entry":{"type":"static","path":"index.html"},"targets":[{"platform":"android","environment":"web"}],"package":{"files":["index.html"],"dependencyLocks":[]}}"#,
        )
        .unwrap();
        std::fs::write(original_source.join("index.html"), "staged bytes").unwrap();
        let staged_source = root
            .path()
            .canonicalize()
            .unwrap()
            .join("local-app-imports/staged-app");
        std::fs::create_dir_all(&staged_source).unwrap();
        std::fs::copy(
            &original_source.join("lingxi-app.json"),
            staged_source.join("lingxi-app.json"),
        )
        .unwrap();
        std::fs::copy(
            &original_source.join("index.html"),
            staged_source.join("index.html"),
        )
        .unwrap();
        std::fs::write(
            original_source.join("index.html"),
            "source changed after staging",
        )
        .unwrap();

        let source_directory = service.guest_path(&original_source).unwrap();
        let authoring = json!({
            "sourceDirectory":source_directory,
            "workspaceId":uuid::Uuid::new_v4().to_string(),
            "projectRef":{"kind":"mobileProject","projectId":uuid::Uuid::new_v4().to_string()},
            "authoringSessionId":uuid::Uuid::new_v4().to_string()
        });
        let session = authoring["authoringSessionId"].as_str().unwrap().to_owned();
        let registered: serde_json::Value = serde_json::from_str(
            &service
                .clone()
                .agent_host_json(
                    json!({"op":"registerAuthoring","sourceId":source_id,"requestId":creation_id,"sessionMode":"code","setCurrent":false,"authoring":authoring}).to_string(),
                )
                .await,
        )
        .unwrap();
        assert_eq!(registered["ok"], true);
        let second = json!({"sourceDirectory":source_directory,"workspaceId":uuid::Uuid::new_v4().to_string(),
            "projectRef":{"kind":"mobileProject","projectId":uuid::Uuid::new_v4().to_string()},
            "authoringSessionId":uuid::Uuid::new_v4().to_string()});
        let registered_second: Value = serde_json::from_str(&service.clone().agent_host_json(
            json!({"op":"registerAuthoring","sourceId":source_id,"requestId":creation_id,"sessionMode":"code","setCurrent":true,"authoring":second}).to_string()).await).unwrap();
        assert_eq!(registered_second["ok"], true);

        let host = MobileProjectAppHost {
            service: service.clone(),
            config_home,
            active_session_uuid: Arc::new(std::sync::Mutex::new(session.clone())),
            catalog: Arc::new(std::sync::OnceLock::new()),
        };
        let publication_id = uuid::Uuid::new_v4().to_string();
        let result = host
            .request(json!({
                "action":"publish",
                "staged_path":staged_source,
                "source_path":original_source,
                "source_session":{"sessionId":session,"mode":"code","requestId":publication_id},
                "expectedRevision":0
            }))
            .await
            .unwrap();
        let app_id = result["id"].as_str().unwrap();
        assert_eq!(result["record"]["authoring"]["appId"], app_id);
        assert_eq!(
            result["record"]["authoring"]["authoringSessionId"],
            second["authoringSessionId"]
        );
        let (receipt, _) = service
            .library
            .native_authoring_publication(&publication_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.publisher.unwrap().authoring_session_id.as_deref(),
            Some(session.as_str())
        );
        assert_eq!(
            result["record"]["authoring"]["sourceDirectory"],
            source_directory
        );
        let release = service.library.release(app_id, 1).unwrap();
        let native = service
            .clone()
            .request_value(json!({"op":"list"}))
            .await
            .unwrap();
        assert_eq!(native[0]["authoring"]["sourceDirectory"], source_directory);
        assert!(native[0]["authoringPublications"].is_array());
        let chat_list = host.request(json!({"action":"list", "source_session":{"sessionId":uuid::Uuid::new_v4().to_string(),"mode":"chat"}})).await.unwrap();
        let model_json = chat_list.to_string();
        for forbidden in [
            "sourceDirectory",
            "sourcePath",
            "authoring",
            "authoringSourceIds",
            "authoringPublications",
        ] {
            assert!(
                !model_json.contains(forbidden),
                "model listing leaked {forbidden}"
            );
        }
        assert_eq!(chat_list[0]["appId"], app_id);
        let index = service
            .library
            .source_path(app_id, 1)
            .unwrap()
            .join("index.html");
        assert_eq!(std::fs::read_to_string(index).unwrap(), "staged bytes");
        assert!(release.files.iter().any(|file| file.path == "index.html"));

        // Exercise the actual model-facing tool in Chat, not just its host
        // projection. Native list above retains the complete record.
        use tool_api::tool_trait::Tool;
        let builtin = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![root.path().to_path_buf()],
        );
        let tool = tool_ui::desktop_local_app::DesktopLocalAppTool::new(
            builtin,
            service.config_home.clone(),
            None,
        )
        .with_host(Arc::new(host));
        let chat_owner = lingxi_core::types::SessionId::new();
        let mut chat_context = tool_api::test_support::fresh_ctx();
        chat_context.origin_session_id = Some(chat_owner);
        chat_context.trusted_session_profile =
            Some(lingxi_core::host::session_context::SessionModeSnapshot {
                session_id: chat_owner.as_uuid().to_string(),
                mode: lingxi_core::host::session_context::ConversationMode::Chat,
            });
        let output = tool
            .call(
                tool_api::tool_trait::generated_builtin_schema(&json!({"action":"list"})),
                chat_context,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap()
            .data
            .to_json_string()
            .unwrap();
        for forbidden in [
            "sourceDirectory",
            "sourcePath",
            "authoring",
            "authoringSourceIds",
            "authoringPublications",
        ] {
            assert!(!output.contains(forbidden), "Chat tool leaked {forbidden}");
        }
        assert!(output.contains(app_id));
    }
}
