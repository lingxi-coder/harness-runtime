//! Host settings snapshots for the interceptable Mod `settings.read` call.

use super::{managed_model_policy_source, settings_watch, DesktopConfig};
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

fn fold(target: &mut Utf16JsonProjection, source: &Utf16JsonProjection) {
    let _ = lingxi_core::settings::merger::merge_raw_source_layer(target, source);
}

fn managed_view(raw_tiers: &[String]) -> Utf16JsonProjection {
    let mut merged = empty();
    for raw in raw_tiers {
        if let Some(source) = parse_valid_settings_source(raw) {
            fold(&mut merged, &source);
        }
    }
    merged
}

fn settings_source(
    settings: &lingxi_core::settings::SettingsJson,
) -> Result<Utf16JsonProjection, ModError> {
    let raw = serde_json::to_string(settings).map_err(|error| ModError::Hook(error.to_string()))?;
    Utf16JsonProjection::parse(&raw).map_err(|error| ModError::Hook(error.to_string()))
}

fn model_allowed_in_managed_tiers(raw_tiers: &[String], model: &str) -> Option<bool> {
    let source = managed_model_policy_source(raw_tiers);
    let enforcement = llm_runtime::model::allowlist::resolve_enforcement(&source, &mut |_| {});
    llm_runtime::model::allowlist::model_allowed_under(&enforcement, model)
}

#[async_trait::async_trait]
impl ModSettingsReader for DesktopModSettingsReader {
    async fn model_allowed(&self, model: &str) -> Result<Option<bool>, ModError> {
        Ok(model_allowed_in_managed_tiers(
            &settings_watch::managed_settings_raw_tiers().await,
            model,
        ))
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
            Some("policy") => Ok(managed_view(
                &settings_watch::managed_settings_raw_tiers().await,
            )),
            None => {
                let managed = settings_watch::managed_settings_raw_tiers().await;
                let mut raw = empty();
                if user_allowed {
                    fold(&mut raw, &read_file(&user).await);
                }
                if project_allowed {
                    fold(&mut raw, &read_file(&project).await);
                    fold(&mut raw, &read_file(&local).await);
                }
                fold(&mut raw, &flag);
                fold(&mut raw, &managed_view(&managed));
                let env: BTreeMap<String, String> = std::env::vars().collect();
                let (env_settings, _) = lingxi_core::settings::env_parser::parse_env(&env)
                    .map_err(|error| ModError::Hook(error.to_string()))?;
                let env_source = settings_source(&env_settings)?;
                fold(&mut raw, &env_source);
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

    #[test]
    fn mod_model_policy_uses_managed_tiers_and_fails_closed() {
        assert_eq!(model_allowed_in_managed_tiers(&[], "claude-opus-4-6"), None);
        let managed = vec![
            r#"{"availableModels":["claude-opus-4-6"],"enforceAvailableModels":true}"#.to_owned(),
        ];
        assert_eq!(
            model_allowed_in_managed_tiers(&managed, "claude-opus-4-6"),
            Some(true)
        );
        assert_eq!(
            model_allowed_in_managed_tiers(&managed, "claude-sonnet-4-6"),
            Some(false)
        );
        assert_eq!(
            model_allowed_in_managed_tiers(&["{".into()], "claude-opus-4-6"),
            Some(false)
        );
    }

    #[test]
    fn policy_tiers_keep_unknown_keys_and_later_values() {
        let tiers = [
            r#"{"enabledPlugins":{"a@org":true},"permissions":{"deny":["Read"],"allow":["Read"]},"custom":"old"}"#.to_owned(),
            r#"{"enabledPlugins":{"b@org":true},"permissions":{"allow":["Read","Bash"]},"custom":"new","allowManagedModsOnly":true}"#.to_owned(),
        ];
        assert_eq!(
            view(&managed_view(&tiers)),
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
        );
        fold(
            &mut merged,
            &source(&json!({"enabledPlugins":{"b@org":true},"custom":{"upper":true},"model":null})),
        );
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
        std::fs::create_dir_all(project.join(".lingxi")).unwrap();
        std::fs::write(home.join("settings.json"), r#"{"custom":"user"}"#).unwrap();
        std::fs::write(
            project.join(".lingxi/settings.json"),
            r#"{"custom":"project"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(".lingxi/settings.local.json"),
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
