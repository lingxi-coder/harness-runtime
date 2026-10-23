//! Native settings commands share Desktop storage, merge and permission writers.
use super::*;
use ::configuration_admin::settings_bridge::{
    apply_patch, build_snapshot, lower_snapshot, permission_destination, permission_paths,
    permission_rule_from_wire,
};
use client::adapter::ClientEventSink;
use client::protocol::commands::{PermissionBehaviorDto, WritableScopeDto};
use client::protocol::text::ClientTextDto;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::{Arc, RwLock};

pub(super) struct MobileModSettingsReader {
    paths: ::configuration_admin::settings_bridge::SettingsPaths,
    active: Arc<RwLock<Utf16JsonProjection>>,
    managed: Utf16JsonProjection,
}

impl MobileModSettingsReader {
    pub(super) fn new(
        paths: ::configuration_admin::settings_bridge::SettingsPaths,
        active: Arc<RwLock<Utf16JsonProjection>>,
        managed: Utf16JsonProjection,
    ) -> Self {
        Self {
            paths,
            active,
            managed,
        }
    }
}

#[async_trait::async_trait]
impl hooks::mods::ModSettingsReader for MobileModSettingsReader {
    async fn read(
        &self,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
        if input.object_entries().is_none() || input.contains_property("source") {
            return Err(hooks::mods::ModError::Protocol(
                "mobile settings reader requires the effective view".into(),
            ));
        }
        let active = self
            .active
            .read()
            .map(|active| active.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
        let snapshot =
            build_snapshot(&self.paths, active, self.managed.clone()).map_err(|error| {
                hooks::mods::ModError::Protocol(
                    error
                        .diagnostic_projection()
                        .to_json_string()
                        .expect("settings diagnostic serializes"),
                )
            })?;
        Ok(snapshot.effective_source)
    }
}

impl MobileEngineHandle {
    pub(super) async fn emit_settings_snapshot(&self, sink: &dyn ClientEventSink) {
        let current_settings = self.current_settings();
        let Some(context) = current_settings.as_ref() else {
            sink.emit(ClientEvent::Error {
                kind: ErrorKindDto::Internal,
                message: "settings listing unavailable: this connection was built without a \
                          settings context"
                    .into(),
            })
            .await;
            return;
        };

        let snapshot = match context.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                sink.emit(ClientEvent::Error {
                    kind: ErrorKindDto::Internal,
                    message: settings_listing_error_message(&error),
                })
                .await;
                return;
            }
        };
        let lowered = match lower_snapshot(&snapshot) {
            Ok(lowered) => lowered,
            Err(error) => {
                sink.emit(ClientEvent::Error {
                    kind: ErrorKindDto::Internal,
                    message: settings_listing_error_message(&error),
                })
                .await;
                return;
            }
        };
        sink.emit(ClientEvent::SettingsSnapshot {
            effective_json: lowered.effective_json,
            provenance_json: lowered.provenance_json,
            files_json: Some(lowered.files_json),
            active_json: Some(lowered.active_json),
            locked: Some(lowered.locked),
            layers_json: Some(lowered.layers_json),
            merged_keys: Some(lowered.merged_keys),
        })
        .await;
    }

    /// Decode `patch_json` via [`parse_settings_patch`] and apply it to
    /// `destination` through [`crate::mobile::settings_bridge::apply_patch`]. On
    /// success, resends the settings snapshot so the caller sees the write it
    /// just made reflected back (rather than requiring a separate
    /// `RefreshListings{Settings}` round-trip). On any failure — no settings
    /// context, a patch that fails to parse (see [`parse_settings_patch`]), or
    /// `apply_patch`'s own errors (reserved key, broken destination file,
    /// write failure) — emits the SAME [`ClientEvent::Error`] path the rest of
    /// this router uses, rather than a dedicated failure event.
    pub(super) async fn apply_settings_patch(
        &self,
        destination: WritableScopeDto,
        patch_json: &str,
        sink: &dyn ClientEventSink,
    ) {
        let current_settings = self.current_settings();
        let Some(context) = current_settings.as_ref() else {
            sink.emit(ClientEvent::Error {
                kind: ErrorKindDto::Internal,
                message: "settings update unavailable: this connection was built without a \
                          settings context"
                    .into(),
            })
            .await;
            return;
        };

        let patch = match parse_settings_patch(patch_json) {
            Ok(patch) => patch,
            Err(message) => {
                sink.emit(ClientEvent::Error {
                    kind: ErrorKindDto::Protocol,
                    message: message.into(),
                })
                .await;
                return;
            }
        };

        let suggestion_setting = patch
            .iter()
            .find(|(key, _)| key == "promptSuggestionEnabled")
            .and_then(|(_, value)| value.as_ref().and_then(serde_json::Value::as_bool));
        match apply_patch(&context.paths, destination, patch) {
            Ok(()) => {
                if let Some(enabled) = suggestion_setting {
                    let _ = self
                        .inner
                        .orchestrator
                        .prompt_suggestion_setting_changed(enabled)
                        .await;
                }
                self.emit_settings_snapshot(sink).await;
            }
            Err(message) => {
                sink.emit(ClientEvent::Error {
                    kind: ErrorKindDto::Internal,
                    message: message.into(),
                })
                .await;
            }
        }
    }

    /// Shared preflight for the three persisted-permission commands
    /// ([`ClientCommand::UpdatePermissionRules`],
    /// [`ClientCommand::SetDefaultPermissionMode`],
    /// [`ClientCommand::UpdateWorkspaceDirectories`]): resolve the active
    /// [`SettingsContext`] into the `permission` crate's two-root paths, or
    /// report the same "no settings context" gap
    /// [`Self::apply_settings_patch`] reports on the generic path.
    pub(super) async fn require_permission_paths(
        &self,
        sink: &dyn ClientEventSink,
    ) -> Option<permission::PermissionPaths> {
        let current_settings = self.current_settings();
        let Some(context) = current_settings.as_ref() else {
            sink.emit(ClientEvent::Error {
                kind: ErrorKindDto::Internal,
                message: "permission update unavailable: this connection was built without a \
                          settings context"
                    .into(),
            })
            .await;
            return None;
        };
        Some(permission_paths(&context.paths))
    }

    /// Route [`ClientCommand::UpdatePermissionRules`] to
    /// `permission::persist_permission_rule_set` — never reimplementing its
    /// per-destination lock, atomic write, or alias-normalizing de-dup. `add`
    /// and `remove` are each persisted in their own call (the persister does
    /// one same-behavior set per transaction); a rule string is never
    /// rejected here — [`permission_rule_from_wire`] parses infallibly,
    /// matching claude-code's own parser.
    pub(super) async fn apply_permission_rule_update(
        &self,
        destination: WritableScopeDto,
        behavior: PermissionBehaviorDto,
        add: Vec<String>,
        remove: Vec<String>,
        sink: &dyn ClientEventSink,
    ) {
        let Some(paths) = self.require_permission_paths(sink).await else {
            return;
        };
        let dest = permission_destination(destination);
        let to_add: Vec<permission::PermissionRule> = add
            .iter()
            .map(|raw| permission_rule_from_wire(raw, behavior, destination))
            .collect();
        let to_remove: Vec<permission::PermissionRule> = remove
            .iter()
            .map(|raw| permission_rule_from_wire(raw, behavior, destination))
            .collect();

        let mut changed = false;
        for (rules, add_flag) in [(&to_add, true), (&to_remove, false)] {
            if rules.is_empty() {
                continue;
            }
            match permission::persist_permission_rule_set(rules, add_flag, dest, &paths).await {
                Ok(did_change) => changed |= did_change,
                Err(error) => {
                    sink.emit(ClientEvent::Error {
                        kind: ErrorKindDto::Internal,
                        message: format!("failed to persist permission rules: {error}").into(),
                    })
                    .await;
                    // `add` and `remove` are two SEPARATE transactions. If the
                    // `add` half already landed durably before this half
                    // errored, the file has genuinely changed — telling the
                    // caller only "it failed" would leave its view of
                    // settings stale and silently wrong. The error still says
                    // the operation did not complete; the snapshot says what
                    // is actually on disk now. Both are true.
                    if changed {
                        self.emit_settings_snapshot(sink).await;
                    }
                    return;
                }
            }
        }

        if changed {
            self.emit_settings_snapshot(sink).await;
        } else {
            // Ok(false) with no error means nothing on disk actually moved —
            // an empty add/remove set, or every entry already matched what
            // was there. Silence here would look identical to a successful
            // write from the caller's side, so it is reported rather than
            // swallowed.
            sink.emit(ClientEvent::Error {
                kind: ErrorKindDto::Rejected,
                message: "no permission rule changed: `add`/`remove` were empty, or every \
                          entry already matched the file"
                    .into(),
            })
            .await;
        }
    }

    /// Route [`ClientCommand::SetDefaultPermissionMode`] to
    /// `permission::persist_permission_mode`. Distinct from the
    /// session-scoped [`ClientCommand::SetPermissionMode`]: this writes the
    /// DEFAULT mode a future session boots into. `persist_permission_mode`
    /// deliberately refuses to persist `"bypassPermissions"` (a security
    /// property — persisting it would silently re-enter bypass mode on the
    /// next session load), and that refusal is reported here rather than
    /// swallowed.
    pub(super) async fn apply_default_permission_mode(
        &self,
        destination: WritableScopeDto,
        mode: String,
        sink: &dyn ClientEventSink,
    ) {
        let Some(paths) = self.require_permission_paths(sink).await else {
            return;
        };
        let dest = permission_destination(destination);
        match permission::persist_permission_mode(&mode, dest, &paths).await {
            Ok(true) => self.emit_settings_snapshot(sink).await,
            Ok(false) if mode == "bypassPermissions" => {
                sink.emit(ClientEvent::Error {
                    kind: ErrorKindDto::Rejected,
                    message: "`bypassPermissions` is session-scoped and is deliberately never \
                              persisted as the default mode"
                        .into(),
                })
                .await;
            }
            Ok(false) => {
                sink.emit(ClientEvent::Error {
                    kind: ErrorKindDto::Rejected,
                    message: format!(
                        "default permission mode was not persisted: `{mode}` is unrecognized, \
                         or already the current default"
                    )
                    .into(),
                })
                .await;
            }
            Err(error) => {
                sink.emit(ClientEvent::Error {
                    kind: ErrorKindDto::Internal,
                    message: format!("failed to persist default permission mode: {error}").into(),
                })
                .await;
            }
        }
    }

    /// Route [`ClientCommand::UpdateWorkspaceDirectories`] to
    /// `permission::persist_workspace_directories`, the same
    /// add-then-remove, report-if-nothing-changed shape as
    /// [`Self::apply_permission_rule_update`].
    pub(super) async fn apply_workspace_directories_update(
        &self,
        destination: WritableScopeDto,
        add: Vec<String>,
        remove: Vec<String>,
        sink: &dyn ClientEventSink,
    ) {
        let Some(paths) = self.require_permission_paths(sink).await else {
            return;
        };
        let dest = permission_destination(destination);

        let mut changed = false;
        for (directories, add_flag) in [(&add, true), (&remove, false)] {
            if directories.is_empty() {
                continue;
            }
            match permission::persist_workspace_directories(directories, add_flag, dest, &paths)
                .await
            {
                Ok(did_change) => changed |= did_change,
                Err(error) => {
                    sink.emit(ClientEvent::Error {
                        kind: ErrorKindDto::Internal,
                        message: format!("failed to persist workspace directories: {error}").into(),
                    })
                    .await;
                    // Same honesty property as `apply_permission_rule_update`:
                    // `add` and `remove` are two SEPARATE transactions, so an
                    // `add` that already landed before `remove` errored is a
                    // real, durable change. Report it alongside the error
                    // rather than leaving the caller's view stale.
                    if changed {
                        self.emit_settings_snapshot(sink).await;
                    }
                    return;
                }
            }
        }

        if changed {
            self.emit_settings_snapshot(sink).await;
        } else {
            sink.emit(ClientEvent::Error {
                kind: ErrorKindDto::Rejected,
                message: "no workspace directory changed: `add`/`remove` were empty, or every \
                          entry already matched the file"
                    .into(),
            })
            .await;
        }
    }

    pub(super) fn mobile_mcp_paths(
        &self,
    ) -> Result<::configuration_admin::mcp_bridge::McpPaths, String> {
        Ok(::configuration_admin::mcp_bridge::McpPaths {
            project_dir: self
                .current_mobile_config()
                .map_err(|error| error.to_string())?
                .cwd,
            global_config_path: self.lingxi_home.join("mcp-config.json"),
        })
    }

    pub(super) async fn apply_mobile_mcp_write(
        &self,
        scope: WritableScopeDto,
        name: &str,
        config: Option<&str>,
    ) {
        let paths = match self.mobile_mcp_paths() {
            Ok(paths) => paths,
            Err(message) => {
                self.connection_sink
                    .emit(ClientEvent::Error {
                        kind: ErrorKindDto::Rejected,
                        message: message.into(),
                    })
                    .await;
                return;
            }
        };
        let result = match config {
            Some(raw) => serde_json::from_str::<serde_json::Value>(raw)
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    value
                        .as_object()
                        .cloned()
                        .ok_or_else(|| "MCP config must be an object".into())
                })
                .and_then(|value| {
                    ::configuration_admin::mcp_bridge::upsert_server(
                        &paths,
                        scope,
                        name,
                        serde_json::Value::Object(value),
                    )
                }),
            None => ::configuration_admin::mcp_bridge::remove_server(&paths, scope, name),
        };
        match result {
            Ok(()) => self.emit_listing(ProtocolListingKind::Mcp).await,
            Err(message) => {
                self.connection_sink
                    .emit(ClientEvent::Error {
                        kind: ErrorKindDto::Internal,
                        message: message.into(),
                    })
                    .await
            }
        }
    }
}

fn settings_listing_error_message(error: &lingxi_core::settings::SettingsError) -> ClientTextDto {
    let mut code_units = "settings listing failed: "
        .encode_utf16()
        .collect::<Vec<_>>();
    code_units.extend(
        error
            .message_projection()
            .string_units("")
            .expect("settings error message is a root string"),
    );
    ClientTextDto::from_code_units(code_units)
}

fn parse_settings_patch(
    patch_json: &str,
) -> Result<Vec<(String, Option<serde_json::Value>)>, String> {
    match serde_json::from_str::<serde_json::Value>(patch_json) {
        Ok(serde_json::Value::Object(map)) => Ok(map
            .into_iter()
            .map(|(k, v)| {
                let v = if v.is_null() { None } else { Some(v) };
                (k, v)
            })
            .collect()),
        Ok(other) => Err(format!(
            "settings patch must be a JSON object, got: {other}"
        )),
        Err(e) => Err(format!("settings patch is not valid JSON: {e}")),
    }
}

/// Move the legacy mobile MCP map into its own storage domain exactly once.
/// The canonical global-config writer serializes concurrent engine starts and
/// publishes atomically; the marker prevents deleted servers being reimported.
pub(super) fn prepare_mobile_mcp_storage(home: &std::path::Path) -> Result<(), String> {
    let target = home.join("mcp-config.json");
    if migrations::global_config::read_map(&target)
        .map_err(|error| error.to_string())?
        .contains_key("mobileSettingsMigrated")
    {
        return Ok(());
    }
    let legacy = migrations::global_config::read_map(&home.join("settings.json"))
        .map_err(|error| error.to_string())?;
    migrations::global_config::save_map(&target, |mut current| {
        if current.contains_key("mobileSettingsMigrated") {
            return current;
        }
        if current.is_empty() {
            for key in ["mcpServers", "projects"] {
                if let Some(value) = legacy.get(key) {
                    current.insert(key.into(), value.clone());
                }
            }
        }
        current.insert(
            "mobileSettingsMigrated".into(),
            serde_json::Value::Bool(true),
        );
        current
    })
    .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_listing_error_keeps_exact_utf16_message_units() {
        let suffix = vec![u16::from(b'e'), 0xD800];
        let projection =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&suffix), suffix.clone())
                .unwrap();
        let error = lingxi_core::settings::SettingsError::source_failure(
            "fixture",
            projection,
            Utf16JsonProjection::object(Vec::new()).unwrap(),
            None,
        );

        let message = settings_listing_error_message(&error);
        let mut expected = "settings listing failed: "
            .encode_utf16()
            .collect::<Vec<_>>();
        expected.extend(suffix);
        assert_eq!(message.code_units, expected);
    }

    #[test]
    fn mcp_migration_preserves_settings_and_does_not_resurrect_deleted_servers() {
        let root = tempfile::tempdir().unwrap();
        let original = r#"{"outputStyle":"concise","mcpServers":{"test":{"command":"echo"}}}"#;
        std::fs::write(root.path().join("settings.json"), original).unwrap();
        prepare_mobile_mcp_storage(root.path()).unwrap();
        let path = root.path().join("mcp-config.json");
        let first = migrations::global_config::read_map(&path).unwrap();
        assert!(first.contains_key("mcpServers"));
        assert!(!first.contains_key("outputStyle"));
        migrations::global_config::save_map(&path, |mut map| {
            map.remove("mcpServers");
            map
        })
        .unwrap();
        prepare_mobile_mcp_storage(root.path()).unwrap();
        assert!(!migrations::global_config::read_map(&path)
            .unwrap()
            .contains_key("mcpServers"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("settings.json")).unwrap(),
            original
        );
    }
    #[test]
    fn settings_patch_requires_an_object_and_null_deletes() {
        assert!(parse_settings_patch("[]").is_err());
        assert!(parse_settings_patch("null").is_err());
        assert_eq!(
            parse_settings_patch(r#"{"outputStyle":null}"#).unwrap(),
            vec![("outputStyle".into(), None)]
        );
    }
}
