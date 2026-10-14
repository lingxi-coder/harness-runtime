//! Host settings snapshots for the interceptable Mod `settings.read` call.

use super::{settings_watch, DesktopConfig};
use hooks::mods::{ModError, ModSettingsReader};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) struct DesktopModSettingsReader {
    cfg: DesktopConfig,
}

impl DesktopModSettingsReader {
    pub(super) fn new(cfg: DesktopConfig) -> Self {
        Self { cfg }
    }

    fn allowed_sources(&self) -> (bool, bool) {
        if self.cfg.restricted {
            (false, false)
        } else {
            self.cfg.setting_source_scope
        }
    }
}

fn empty() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("{}").expect("empty object is valid JSON")
}

fn parse_valid_settings_source(raw: &str) -> Option<Utf16JsonProjection> {
    let typed = serde_json::from_str::<lingxi_core::settings::SettingsJson>(raw).ok()?;
    if typed.validate().is_err() {
        return None;
    }
    let source = Utf16JsonProjection::parse(raw).ok()?;
    source.object_entries().is_some().then_some(source)
}

async fn read_file(path: &Path) -> Utf16JsonProjection {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(_) => return empty(),
    };
    if !metadata.is_file()
        || metadata.len() > lingxi_core::settings::loader::MAX_SETTINGS_FILE_BYTES
    {
        return empty();
    }
    let Ok(data) = tokio::fs::read(path).await else {
        return empty();
    };
    if u64::try_from(data.len()).unwrap_or(u64::MAX)
        > lingxi_core::settings::loader::MAX_SETTINGS_FILE_BYTES
    {
        return empty();
    }
    let Ok(raw) = std::str::from_utf8(&data) else {
        return empty();
    };
    parse_valid_settings_source(raw).unwrap_or_else(empty)
}

fn fold(target: &mut Utf16JsonProjection, source: &Utf16JsonProjection) -> Result<(), ModError> {
    lingxi_core::settings::merger::merge_raw_source_layer(target, source)
        .map(|_| ())
        .map_err(|error| ModError::Hook(error.to_string()))
}

fn managed_view(
    snapshot: &settings_watch::ManagedSettingsSnapshot,
) -> Result<Utf16JsonProjection, ModError> {
    if snapshot.read_failed() {
        return Err(ModError::Hook(
            "managed policy source selection is incomplete".into(),
        ));
    }
    Ok(snapshot
        .effective_projection()
        .cloned()
        .unwrap_or_else(empty))
}

fn settings_source(
    settings: &lingxi_core::settings::SettingsJson,
) -> Result<Utf16JsonProjection, ModError> {
    let raw = serde_json::to_string(settings).map_err(|error| ModError::Hook(error.to_string()))?;
    Utf16JsonProjection::parse(&raw).map_err(|error| ModError::Hook(error.to_string()))
}

fn catalog_restriction_for_settings(
    cfg: &DesktopConfig,
    snapshot: &settings_watch::ManagedSettingsSnapshot,
) -> Result<
    (
        llm_runtime::model::allowlist::CatalogRestriction,
        Option<BTreeMap<String, String>>,
    ),
    ModError,
> {
    use llm_runtime::model::allowlist::{catalog_restriction_from_settings, CatalogRestriction};

    if snapshot.read_failed() {
        return Ok((CatalogRestriction::Refused, None));
    }
    let effective =
        super::load_effective_settings_for_config(cfg, Some(snapshot)).map_err(|error| {
            ModError::Hook(
                error
                    .diagnostic_projection()
                    .to_json_string()
                    .expect("valid diagnostic JSON"),
            )
        })?;
    Ok((
        catalog_restriction_from_settings(true, effective.settings.available_models.clone()),
        Some(effective.settings.model_overrides.unwrap_or_default()),
    ))
}

fn model_allowed_in_settings(
    cfg: &DesktopConfig,
    snapshot: &settings_watch::ManagedSettingsSnapshot,
    model: &str,
) -> Result<Option<bool>, ModError> {
    use llm_runtime::model::allowlist::{model_allowed_under_catalog, CatalogRestriction};

    let (restriction, overrides) = catalog_restriction_for_settings(cfg, snapshot)?;
    Ok(match restriction {
        CatalogRestriction::Unrestricted => None,
        restriction => Some(model_allowed_under_catalog(
            model,
            &restriction,
            overrides.as_ref(),
        )),
    })
}

fn model_allowed_in_settings_projection(
    cfg: &DesktopConfig,
    snapshot: &settings_watch::ManagedSettingsSnapshot,
    model: &Utf16JsonProjection,
) -> Result<Option<bool>, ModError> {
    use llm_runtime::model::allowlist::{model_allowed_under_catalog_utf16, CatalogRestriction};

    let units = model
        .string_units("")
        .ok_or_else(|| ModError::Hook("model source must be a JSON string".into()))?;
    let (restriction, overrides) = catalog_restriction_for_settings(cfg, snapshot)?;
    Ok(match restriction {
        CatalogRestriction::Unrestricted => None,
        restriction => Some(model_allowed_under_catalog_utf16(
            &units,
            &restriction,
            overrides.as_ref(),
        )),
    })
}

#[async_trait::async_trait]
impl ModSettingsReader for DesktopModSettingsReader {
    async fn model_allowed(&self, model: &str) -> Result<Option<bool>, ModError> {
        let snapshot = settings_watch::managed_settings_snapshot_for_config(&self.cfg).await;
        model_allowed_in_settings(&self.cfg, &snapshot, model)
    }

    async fn model_allowed_projection(
        &self,
        model: &Utf16JsonProjection,
    ) -> Result<Option<bool>, ModError> {
        let snapshot = settings_watch::managed_settings_snapshot_for_config(&self.cfg).await;
        model_allowed_in_settings_projection(&self.cfg, &snapshot, model)
    }

    async fn read(&self, input: Utf16JsonProjection) -> Result<Utf16JsonProjection, ModError> {
        if input.object_entries().is_none() {
            return Err(ModError::Hook(
                "settings.read takes an options object".into(),
            ));
        }
        let source = if input.contains_property("source") {
            match input.string_at("/source") {
                Some(source)
                    if matches!(source, "user" | "project" | "local" | "flag" | "policy") =>
                {
                    Some(source)
                }
                _ => return Err(ModError::Hook("settings.read has an unknown source".into())),
            }
        } else {
            None
        };
        let (user_allowed, project_allowed) = self.allowed_sources();
        let user = self.cfg.lingxi_home.join("settings.json");
        let project = lingxi_core::settings::loader::project_settings_path(&self.cfg.cwd);
        let local = lingxi_core::settings::loader::local_settings_path(&self.cfg.cwd);
        let flag = self
            .cfg
            .flag_settings
            .as_ref()
            .map(settings_source)
            .transpose()?
            .unwrap_or_else(empty);
        match source {
            Some("user") => {
                if user_allowed {
                    Ok(read_file(&user).await)
                } else {
                    Ok(empty())
                }
            }
            Some("project") => {
                if project_allowed {
                    Ok(read_file(&project).await)
                } else {
                    Ok(empty())
                }
            }
            Some("local") => {
                if project_allowed {
                    Ok(read_file(&local).await)
                } else {
                    Ok(empty())
                }
            }
            Some("flag") => Ok(flag),
            Some("policy") => {
                managed_view(&settings_watch::managed_settings_snapshot_for_config(&self.cfg).await)
            }
            None => {
                let managed = settings_watch::managed_settings_snapshot_for_config(&self.cfg).await;
                let mut raw = empty();
                if user_allowed {
                    fold(&mut raw, &read_file(&user).await)?;
                }
                if project_allowed {
                    fold(&mut raw, &read_file(&project).await)?;
                    fold(&mut raw, &read_file(&local).await)?;
                }
                fold(&mut raw, &flag)?;
                fold(&mut raw, &managed_view(&managed)?)?;
                let env: BTreeMap<String, String> = std::env::vars().collect();
                let (env_settings, _) = lingxi_core::settings::env_parser::parse_env(&env)
                    .map_err(|error| ModError::Hook(error.to_string()))?;
                let env_source = settings_source(&env_settings)?;
                fold(&mut raw, &env_source)?;
                Ok(raw)
            }
            Some(_) => unreachable!("source validated above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn source(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).unwrap()
    }

    fn view(value: &Utf16JsonProjection) -> Value {
        serde_json::from_str(&value.to_json_string().unwrap()).unwrap()
    }

    fn read_options(source_name: &str) -> Utf16JsonProjection {
        source(&json!({"source": source_name}))
    }

    #[tokio::test]
    async fn model_admission_uses_merged_settings_and_fails_closed_on_policy_read_errors() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let cfg = DesktopConfig {
            cwd,
            lingxi_home: home.clone(),
            setting_source_scope: (true, true),
            ..Default::default()
        };
        let settings = root.path().join("managed-settings.json");
        let absent = settings_watch::read_managed_settings_snapshot(root.path()).await;
        std::fs::write(
            home.join("settings.json"),
            r#"{"availableModels":["claude-opus-4-6"]}"#,
        )
        .unwrap();
        assert_eq!(
            model_allowed_in_settings(&cfg, &absent, "claude-opus-4-6").unwrap(),
            Some(true)
        );
        assert_eq!(
            model_allowed_in_settings(&cfg, &absent, "claude-sonnet-4-6").unwrap(),
            Some(false)
        );

        std::fs::write(
            &settings,
            r#"{"availableModels":["claude-sonnet-4-6"],"enforceAvailableModels":false}"#,
        )
        .unwrap();
        let managed = settings_watch::read_managed_settings_snapshot(root.path()).await;
        assert_eq!(
            model_allowed_in_settings(&cfg, &managed, "claude-opus-4-6").unwrap(),
            Some(false)
        );
        assert_eq!(
            model_allowed_in_settings(&cfg, &managed, "claude-sonnet-4-6").unwrap(),
            Some(true)
        );

        std::fs::write(&settings, "{").unwrap();
        let unresolved = settings_watch::read_managed_settings_snapshot(root.path()).await;
        assert_eq!(
            model_allowed_in_settings(&cfg, &unresolved, "claude-opus-4-6").unwrap(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn policy_selection_keeps_unknown_keys_and_uses_source_merge_order() {
        let root = tempfile::tempdir().unwrap();
        let drop_ins = root.path().join("managed-settings.d");
        std::fs::create_dir_all(&drop_ins).unwrap();
        std::fs::write(
            root.path().join("managed-settings.json"),
            r#"{"enabledPlugins":{"a@org":true},"permissions":{"deny":["Read"],"allow":["Read"]},"custom":"old"}"#,
        )
        .unwrap();
        std::fs::write(
            drop_ins.join("10-org.json"),
            r#"{"enabledPlugins":{"b@org":true},"permissions":{"allow":["Read","Bash"]},"custom":"new","allowManagedModsOnly":true}"#,
        )
        .unwrap();
        let snapshot = settings_watch::read_managed_settings_snapshot(root.path()).await;
        assert_eq!(
            view(&managed_view(&snapshot).unwrap()),
            json!({
                "enabledPlugins":{"a@org":true,"b@org":true},
                "permissions":{"deny":["Read"],"allow":["Read","Bash"]},
                "custom":"new",
                "allowManagedModsOnly":true,
            })
        );
    }

    #[test]
    fn raw_fold_preserves_known_merge_and_unknown_override() {
        let mut merged = empty();
        fold(
            &mut merged,
            &source(&json!({"enabledPlugins":{"a@org":true},"custom":{"lower":true},"model":"a"})),
        )
        .expect("fixture settings layers are mergeable");
        fold(
            &mut merged,
            &source(&json!({"enabledPlugins":{"b@org":true},"custom":{"upper":true},"model":null})),
        )
        .expect("fixture settings layers are mergeable");
        assert_eq!(
            view(&merged),
            json!({
                "enabledPlugins":{"a@org":true,"b@org":true},
                "custom":{"upper":true},
                "model":"a",
            })
        );
    }

    #[tokio::test]
    async fn source_reads_preserve_raw_fields_and_respect_scope() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();
        std::fs::write(home.join("settings.json"), r#"{"custom":"user"}"#).unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.json"),
            r#"{"custom":"project"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.local.json"),
            r#"{"custom":"local"}"#,
        )
        .unwrap();
        let mut cfg = DesktopConfig::default();
        cfg.cwd = project;
        cfg.lingxi_home = home;
        let reader = DesktopModSettingsReader::new(cfg.clone());
        for (source, expected) in [("user", "user"), ("project", "project"), ("local", "local")] {
            let result = reader.read(read_options(source)).await.unwrap();
            assert_eq!(result.string_at("/custom"), Some(expected));
        }
        cfg.setting_source_scope = (false, false);
        let restricted = DesktopModSettingsReader::new(cfg);
        for source in ["user", "project", "local"] {
            assert_eq!(
                view(&restricted.read(read_options(source)).await.unwrap()),
                json!({}),
            );
        }
        assert!(restricted.read(read_options("unknown")).await.is_err());
        std::fs::write(reader.cfg.lingxi_home.join("settings.json"), "{broken").unwrap();
        assert_eq!(
            view(&reader.read(read_options("user")).await.unwrap()),
            json!({})
        );
        std::fs::write(
            reader.cfg.lingxi_home.join("settings.json"),
            r#"{"pluginConfigs":{"plugin":{"options":{"custom":"\ud800"}}}}"#,
        )
        .unwrap();
        let source_preserving = reader.read(read_options("user")).await.unwrap();
        assert_eq!(
            source_preserving.string_units("/pluginConfigs/plugin/options/custom"),
            Some(vec![0xD800])
        );
    }
}
