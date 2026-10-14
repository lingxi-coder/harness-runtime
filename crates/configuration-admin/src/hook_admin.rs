use hooks::{parse_hook_layer_strict, HookSource};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use migrations::settings_update::{read_settings_source, settings_path, WritableScope};
use plugin::plugin_source_sha256;
use serde::Serialize;
use serde_json::value::RawValue;
use std::collections::BTreeMap;

use crate::settings_bridge::{apply_patch_source, settings_error_message, SettingsContext};
use client::protocol::commands::WritableScopeDto;

pub struct HookRuntimeCandidate {
    pub source: HookSource,
    pub hooks: Vec<hooks::HookDefinition>,
}

#[derive(Debug, Serialize)]
struct HookDocument {
    scope: String,
    revision_sha256: String,
    own_json: String,
    effective_json: String,
}

struct HookAdminPayload {
    scope: Option<String>,
    hooks: Box<RawValue>,
}

pub fn document_json(context: &SettingsContext, scope: Option<&str>) -> Result<String, String> {
    let scope = scope.unwrap_or("local");
    let snapshot = context
        .snapshot()
        .map_err(|error| settings_error_message(&error))?;
    let own = snapshot
        .layers
        .get(scope)
        .map(hooks_from_settings_source)
        .transpose()?
        .unwrap_or_else(empty_json_object);
    let own_json = own
        .to_pretty_json_string()
        .map_err(|error| error.to_string())?;
    let effective_hooks = hooks_from_settings_source(&snapshot.effective_source)?;
    let effective_json = effective_hooks
        .to_pretty_json_string()
        .map_err(|error| error.to_string())?;
    let document = HookDocument {
        scope: scope.to_string(),
        revision_sha256: plugin_source_sha256(own_json.as_bytes()),
        own_json,
        effective_json,
    };
    serde_json::to_string(&document).map_err(|error| error.to_string())
}

pub fn validate_document(payload_json: &str) -> Result<(), String> {
    runtime_candidate(payload_json).map(|_| ())
}

pub fn runtime_candidate(payload_json: &str) -> Result<HookRuntimeCandidate, String> {
    let payload = parse_payload(payload_json)?;
    let scope = required_scope(&payload)?;
    let source = hook_source(scope)?;
    if payload.hooks.get().trim() == "null" {
        return Ok(HookRuntimeCandidate {
            source,
            hooks: Vec::new(),
        });
    }
    parse_hook_layer_strict(payload.hooks.get(), source)
        .map(|parsed| HookRuntimeCandidate {
            source,
            hooks: parsed.hooks,
        })
        .map_err(|error| format!("invalid hooks payload: {error}"))
}

pub fn save_document(
    context: &SettingsContext,
    revision_sha256: &str,
    payload_json: &str,
) -> Result<(), String> {
    let payload = parse_payload(payload_json)?;
    let scope = required_scope(&payload)?;
    validate_document(payload_json)?;
    let destination = settings_destination(scope)?;
    let current = own_hooks_projection(context, scope);
    let current_json = current
        .to_pretty_json_string()
        .map_err(|error| error.to_string())?;
    let actual = plugin_source_sha256(current_json.as_bytes());
    if actual != revision_sha256 {
        return Err(format!(
            "revision conflict: expected {revision_sha256}, found {actual}"
        ));
    }
    let hooks = if payload.hooks.get().trim() == "null" {
        None
    } else {
        let parsed = parse_hook_layer_strict(payload.hooks.get(), hook_source(scope)?)
            .map_err(|error| format!("invalid hooks payload: {error}"))?;
        if parsed.document.events.is_empty() {
            None
        } else {
            let serialized = serde_json::to_string(&parsed.document)
                .map_err(|error| format!("failed to serialize hooks payload: {error}"))?;
            Some(
                Utf16JsonProjection::parse(&serialized)
                    .map_err(|error| format!("failed to serialize hooks payload: {error}"))?,
            )
        }
    };
    apply_patch_source(
        &context.paths,
        destination,
        vec![("hooks".to_string(), hooks)],
    )
}

fn own_hooks_projection(context: &SettingsContext, scope: &str) -> Utf16JsonProjection {
    let source = match scope {
        "user" => WritableScope::User,
        "project" => WritableScope::Project,
        "local" => WritableScope::Local,
        _ => return empty_json_object(),
    };
    let path = settings_path(
        source,
        &context.paths.lingxi_home,
        &context.paths.project_dir,
    );
    let settings = match read_settings_source(&path) {
        Ok(settings) => settings,
        Err(_) => return empty_json_object(),
    };
    hooks_from_settings_source(&settings).unwrap_or_else(|_| empty_json_object())
}

fn hooks_from_settings_source(
    settings: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, String> {
    if settings.get_property("hooks").is_none() {
        return Ok(empty_json_object());
    }
    settings
        .subprojection("/hooks")
        .map_err(|error| error.to_string())
}

fn empty_json_object() -> Utf16JsonProjection {
    Utf16JsonProjection::object(Vec::new()).expect("empty JSON object is valid")
}

fn parse_payload(payload_json: &str) -> Result<HookAdminPayload, String> {
    let projection = Utf16JsonProjection::parse(payload_json)
        .map_err(|error| format!("payload is not valid JSON: {error}"))?;
    if projection.object_entries().is_none() {
        return Err("payload must be a JSON object".to_string());
    }
    if projection.get_property("hooks").is_none() {
        return Err("payload is missing `hooks`".to_string());
    }
    let mut fields = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(payload_json)
        .map_err(|error| format!("payload is not valid JSON: {error}"))?;
    let scope: Option<String> = fields
        .remove("scope")
        .and_then(|raw| serde_json::from_str(raw.get()).ok());
    let hooks = fields
        .remove("hooks")
        .ok_or_else(|| "payload is missing `hooks`".to_string())?;
    Ok(HookAdminPayload { scope, hooks })
}

fn required_scope(payload: &HookAdminPayload) -> Result<&str, String> {
    payload
        .scope
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "payload is missing `scope`".to_string())
}

fn settings_destination(scope: &str) -> Result<WritableScopeDto, String> {
    match scope {
        "user" => Ok(WritableScopeDto::User),
        "project" => Ok(WritableScopeDto::Project),
        "local" => Ok(WritableScopeDto::Local),
        _ => Err(format!("unknown hook scope `{scope}`")),
    }
}

fn hook_source(scope: &str) -> Result<HookSource, String> {
    match scope {
        "user" => Ok(HookSource::Settings(
            lingxi_core::types::SettingsScope::User,
        )),
        "project" => Ok(HookSource::Settings(
            lingxi_core::types::SettingsScope::Project,
        )),
        "local" => Ok(HookSource::Settings(
            lingxi_core::types::SettingsScope::Local,
        )),
        _ => Err(format!("unknown hook scope `{scope}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_bridge::SettingsPaths;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{Arc, RwLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn context() -> (SettingsContext, std::path::PathBuf) {
        // The per-test root must be unique WITHIN this binary, not merely
        // per-process: `saving_an_empty_document_removes_only_the_hooks_key`
        // and `document_and_revision_are_scoped_to_the_layer_hooks_object`
        // run on different threads of the SAME process, so `process::id()`
        // is identical for both and `SystemTime::now()` can return the same
        // value to both when they start inside one clock tick. On that
        // collision the second test's closing `remove_dir_all(root)` deletes
        // the first test's `settings.json` mid-test, and the save fails with
        // `revision conflict: … found <sha256 of "">` — the empty-document
        // hash — for a document the test had just written. Observed once in
        // a full parallel run (round 12) and never in four reruns of this
        // binary alone, which is exactly the shape of a start-time collision.
        // The counter makes the root unique by construction.
        static NEXT_ROOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "lingxi-hook-admin-{}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let home = root.join("home");
        let project = root.join("project");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&project).expect("project");
        (
            SettingsContext {
                fusion_layers: None,
                paths: SettingsPaths {
                    lingxi_home: home,
                    project_dir: project,
                },
                active: Arc::new(RwLock::new(crate::settings_bridge::empty_settings_source())),
                managed: crate::settings_bridge::empty_settings_source(),
                managed_diagnostics: None,
            },
            root,
        )
    }

    #[test]
    fn document_and_revision_are_scoped_to_the_layer_hooks_object() {
        let (context, root) = context();
        fs::write(
            context.paths.lingxi_home.join("settings.json"),
            r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        )
        .expect("settings");

        let document: Value =
            serde_json::from_str(&document_json(&context, Some("user")).expect("document"))
                .expect("json");
        let own: Value =
            serde_json::from_str(document["own_json"].as_str().expect("own")).expect("own json");
        assert!(own.get("Stop").is_some());
        assert!(own.get("theme").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn effective_document_cold_read_keeps_lone_utf16_and_sibling_settings() {
        let (context, root) = context();
        fs::write(
            context.paths.lingxi_home.join("settings.json"),
            r#"{"model":"opus","hooks":{"PostToolUse":[{"hooks":[{"type":"mcp_tool","server":"audit","tool":"check","input":{"value":"\ud800"}}]}]},"opaque":{"value":"\udfff"}}"#,
        )
        .expect("settings source");

        let document: Value =
            serde_json::from_str(&document_json(&context, Some("user")).expect("document"))
                .expect("document JSON");
        let effective = Utf16JsonProjection::parse(
            document["effective_json"]
                .as_str()
                .expect("effective JSON string"),
        )
        .expect("effective source JSON");
        assert_eq!(effective.string_at("/model"), None);
        assert_eq!(
            effective.string_units("/PostToolUse/0/hooks/0/input/value"),
            Some(vec![0xD800])
        );
        assert_eq!(effective.string_units("/opaque/value"), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn saving_an_empty_document_removes_only_the_hooks_key() {
        let (context, root) = context();
        let path = context.paths.lingxi_home.join("settings.json");
        fs::write(
            &path,
            r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        )
        .expect("settings");
        let document: Value =
            serde_json::from_str(&document_json(&context, Some("user")).expect("document"))
                .expect("json");
        let revision = document["revision_sha256"].as_str().expect("revision");

        save_document(&context, revision, r#"{"scope":"user","hooks":{}}"#).expect("save");
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("saved json");
        assert_eq!(saved["theme"], "dark");
        assert!(saved.get("hooks").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn null_document_builds_an_empty_runtime_candidate() {
        let candidate = runtime_candidate(r#"{"scope":"local","hooks":null}"#).expect("candidate");
        assert_eq!(
            candidate.source,
            HookSource::Settings(lingxi_core::types::SettingsScope::Local)
        );
        assert!(candidate.hooks.is_empty());
    }

    #[test]
    fn runtime_candidate_keeps_raw_mcp_input_utf16_units() {
        let candidate = runtime_candidate(
            r#"{"scope":"local","hooks":{"PostToolUse":[{"hooks":[{"type":"mcp_tool","server":"audit","tool":"lint","input":{"path":"\ud800"}}]}]}}"#,
        )
        .expect("candidate");
        let hooks::HookExecutor::McpTool { input, .. } = &candidate.hooks[0].executor else {
            panic!("expected MCP hook executor");
        };
        assert_eq!(
            input.string_units("/path").as_deref(),
            Some([0xD800].as_slice())
        );
    }

    #[test]
    fn saving_hooks_keeps_lone_surrogate_mcp_input_source() {
        let (context, root) = context();
        let document: Value =
            serde_json::from_str(&document_json(&context, Some("user")).expect("document"))
                .expect("JSON document");
        let revision = document["revision_sha256"].as_str().expect("revision");
        save_document(
            &context,
            revision,
            r#"{"scope":"user","hooks":{"PostToolUse":[{"hooks":[{"type":"mcp_tool","server":"audit","tool":"lint","input":{"path":"\ud800"}}]}]}}"#,
        )
        .expect("Source settings write");

        let path = settings_path(
            WritableScope::User,
            &context.paths.lingxi_home,
            &context.paths.project_dir,
        );
        let source = read_settings_source(&path).expect("source settings file");
        assert_eq!(
            source.string_units("/hooks/PostToolUse/0/hooks/0/input/path"),
            Some(vec![0xD800])
        );
        let serialized = std::fs::read_to_string(path).expect("saved settings text");
        assert!(serialized.contains(r#""path": "\ud800""#));
        let snapshot = context.snapshot().unwrap();
        assert!(snapshot.files[0].parse_error.is_none());
        let document: Value = serde_json::from_str(
            &document_json(&context, Some("user")).expect("cold-read document"),
        )
        .expect("document JSON");
        let effective = Utf16JsonProjection::parse(
            document["effective_json"].as_str().expect("effective JSON"),
        )
        .expect("cold-read effective source");
        assert_eq!(
            effective.string_units("/PostToolUse/0/hooks/0/input/path"),
            Some(vec![0xD800])
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
