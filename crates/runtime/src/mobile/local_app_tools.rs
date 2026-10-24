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
    /// Metadata only; this factory is never bound as the application's executor.
    pub catalog: Arc<std::sync::OnceLock<std::sync::Weak<super::app_agent::AppAgentFactory>>>,
}

struct ImportDirectory(PathBuf);
impl Drop for ImportDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl MobileProjectAppHost {
    async fn dispatch(&self, request: Value) -> Result<Value, String> {
        self.service
            .clone()
            .request_for_tool(request)
            .await
            .map_err(|error| format!("{}: {}", error.code, error.message))
    }

    fn guest_source(&self, source: &str) -> Result<String, String> {
        let home = self.config_home.parent().ok_or("Missing Linux user home")?;
        let source = std::fs::canonicalize(source).map_err(|error| error.to_string())?;
        let home = std::fs::canonicalize(home).map_err(|error| error.to_string())?;
        let relative = source
            .strip_prefix(home)
            .map_err(|_| "Application source must be in the Linux user home")?;
        Ok(Path::new(mobile_linux_api::guest_paths::HOME)
            .join(relative)
            .to_string_lossy()
            .into_owned())
    }
}

#[async_trait]
impl ProjectLocalAppHost for MobileProjectAppHost {
    async fn request(&self, input: Value) -> Result<Value, String> {
        let app_id = input["id"].as_str();
        match input["action"].as_str().unwrap_or("publish") {
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
            "list" => self.dispatch(json!({"op":"list"})).await,
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
                self.dispatch(json!({"op":input["action"],"appId":app_id.ok_or("Application id is required")?})).await
            }
            "publish" => {
                let mut temporary = None;
                let source = if let Some(staged) = input["staged_path"].as_str() { staged.to_owned() }
                else if let Some(html) = input["html"].as_str() {
                    let report = local_app::checks::check_document(html);
                    // The host does not silently publish malformed documents.
                    if !report.errors.is_empty() { return Err(report.errors.join("; ")); }
                    let path = self.config_home.join("local-app-imports").join(uuid::Uuid::new_v4().simple().to_string());
                    let guard = ImportDirectory(path.clone());
                    std::fs::create_dir_all(&path).map_err(|error|error.to_string())?;
                    let name = input["name"].as_str().unwrap_or("Local App");
                    let manifest = json!({"schema":2,"name":name,"summary":input["summary"].as_str().unwrap_or(""),"entry":{"type":"static","path":"index.html"},"targets":[{"platform":"android","environment":"web"},{"platform":"ios","environment":"web"},{"platform":"macos","environment":"web"},{"platform":"windows","environment":"web"},{"platform":"linux","environment":"web"},{"platform":"web","environment":"web"}],"package":{"files":["index.html"],"dependencyLocks":[]}});
                    std::fs::write(path.join("index.html"),html).map_err(|error|error.to_string())?;
                    std::fs::write(path.join("lingxi-app.json"),manifest.to_string()).map_err(|error|error.to_string())?;
                    temporary = Some(guard);
                    path.to_string_lossy().into_owned()
                } else { return Err("A staged project or HTML document is required".into()); };
                let authoring = if app_id.is_none() {
                    input["source_path"].as_str().map(|source|self.guest_source(source).map(|directory|json!({"sourceDirectory":directory,"sourceSessionId":input["source_session"]["sessionId"]}))).transpose()?
                } else { None };
                let result = self.dispatch(json!({"op":"publish","appId":app_id,"expectedRevision":input["expectedRevision"].as_u64().unwrap_or(0),"sourcePath":source,"authoring":authoring})).await?;
                drop(temporary);
                Ok(json!({"action":"publish","id":result["record"]["appId"],"name":result["record"]["name"],"summary":result["record"]["summary"],"revision":result["release"]["revision"],"record":result["record"],"release":result["release"]}))
            }
            _ => Err("Unknown Local App action; consent and administration are native-only".into()),
        }
    }
}
