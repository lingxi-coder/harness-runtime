use hooks::{parse_hook_layer_value_strict, serialize_hook_layer, HookSource};
use plugin::plugin_source_sha256;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::settings_bridge::{apply_patch, build_snapshot, SettingsContext};
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

pub fn document_json(context: &SettingsContext, scope: Option<&str>) -> Result<String, String> {
    let scope = scope.unwrap_or("local");
    let snapshot = build_snapshot(
        &context.paths,
        context.active_snapshot(),
        context.managed.clone(),
    );
    let own = snapshot
        .layers
        .get(scope)
        .and_then(|layer| layer.get("hooks"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let own_json = serde_json::to_string_pretty(&own).map_err(|error| error.to_string())?;
    let effective = snapshot
        .effective
        .get("hooks")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let effective_json =
        serde_json::to_string_pretty(&effective).map_err(|error| error.to_string())?;
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
    let scope = required_string(&payload, "scope")?;
    let source = hook_source(scope)?;
    let hooks = payload
        .get("hooks")
        .cloned()
        .ok_or_else(|| "payload is missing `hooks`".to_string())?;
    if hooks.is_null() {
        return Ok(HookRuntimeCandidate {
            source,
            hooks: Vec::new(),
        });
    }
    parse_hook_layer_value_strict(&hooks, hook_source(scope)?)
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
    let scope = required_string(&payload, "scope")?;
    validate_document(payload_json)?;
    let destination = settings_destination(scope)?;
    let snapshot = build_snapshot(
        &context.paths,
        context.active_snapshot(),
        context.managed.clone(),
    );
    let layer_name = scope;
    let current = snapshot
        .layers
        .get(layer_name)
        .and_then(|layer| layer.get("hooks"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let current_json = serde_json::to_string_pretty(&current).map_err(|error| error.to_string())?;
    let actual = plugin_source_sha256(current_json.as_bytes());
    if actual != revision_sha256 {
        return Err(format!(
            "revision conflict: expected {revision_sha256}, found {actual}"
        ));
    }
    let hooks = payload
        .get("hooks")
        .cloned()
        .ok_or_else(|| "payload is missing `hooks`".to_string())?;
    let hooks = if hooks.is_null() {
        None
    } else {
        let parsed = parse_hook_layer_value_strict(&hooks, hook_source(scope)?)
            .map_err(|error| format!("invalid hooks payload: {error}"))?;
        if parsed.document.events.is_empty() {
            None
        } else {
            Some(
                serialize_hook_layer(&parsed.document)
                    .map_err(|error| format!("failed to serialize hooks payload: {error}"))?,
            )
        }
    };
    apply_patch(
        &context.paths,
        destination,
        vec![("hooks".to_string(), hooks)],
    )
}

fn parse_payload(payload_json: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(payload_json) {
        Ok(Value::Object(payload)) => Ok(payload),
        Ok(_) => Err("payload must be a JSON object".to_string()),
        Err(error) => Err(format!("payload is not valid JSON: {error}")),
    }
}

fn required_string<'a>(payload: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("payload is missing `{key}`"))
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
        "user" => Ok(HookSource::Settings(protocol::SettingsScope::User)),
        "project" => Ok(HookSource::Settings(protocol::SettingsScope::Project)),
        "local" => Ok(HookSource::Settings(protocol::SettingsScope::Local)),
        _ => Err(format!("unknown hook scope `{scope}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_bridge::SettingsPaths;
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
                paths: SettingsPaths {
                    lingxi_home: home,
                    project_dir: project,
                },
                active: Arc::new(RwLock::new(BTreeMap::new())),
                managed: BTreeMap::new(),
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
            HookSource::Settings(protocol::SettingsScope::Local)
        );
        assert!(candidate.hooks.is_empty());
    }
}
