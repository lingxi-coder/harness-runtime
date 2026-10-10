//! Host settings snapshots for the interceptable Mod `settings.read` call.

use super::{managed_model_policy_source, settings_watch, DesktopConfig};
use hooks::mods::{ModError, ModSettingsReader};
use serde_json::{Map, Value};
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

fn empty() -> Value {
    Value::Object(Map::new())
}

async fn read_file(path: &Path) -> Value {
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
    let Ok(typed) = serde_json::from_slice::<lingxi_core::settings::SettingsJson>(&data) else {
        return empty();
    };
    if typed.validate().is_err() {
        return empty();
    }
    serde_json::from_slice::<Value>(&data)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(empty)
}

fn fold(target: &mut BTreeMap<String, Value>, source: Value) {
    if let Value::Object(source) = source {
        let _ =
            lingxi_core::settings::merger::merge_raw_layer(target, source.into_iter().collect());
    }
}

fn managed_view(raw_tiers: &[String]) -> Value {
    let mut merged = BTreeMap::new();
    for raw in raw_tiers {
        if let (Ok(typed), Ok(value)) = (
            serde_json::from_str::<lingxi_core::settings::SettingsJson>(raw),
            serde_json::from_str::<Value>(raw),
        ) {
            if typed.validate().is_ok() {
                fold(&mut merged, value);
            }
        }
    }
    Value::Object(merged.into_iter().collect())
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

    async fn read(&self, input: Value) -> Result<Value, ModError> {
        let args = input
            .as_object()
            .ok_or_else(|| ModError::Hook("settings.read takes an options object".into()))?;
        let source = match args.get("source") {
            None => None,
            Some(Value::String(source))
                if matches!(
                    source.as_str(),
                    "user" | "project" | "local" | "flag" | "policy"
                ) =>
            {
                Some(source.as_str())
            }
            _ => return Err(ModError::Hook("settings.read has an unknown source".into())),
        };
        let (user_allowed, project_allowed) = self.allowed_sources();
        let user = self.cfg.lingxi_home.join("settings.json");
        let project = lingxi_core::settings::loader::project_settings_path(&self.cfg.cwd);
        let local = lingxi_core::settings::loader::local_settings_path(&self.cfg.cwd);
        let flag = self
            .cfg
            .flag_settings
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| ModError::Hook(error.to_string()))?
            .unwrap_or_else(|| Value::Object(Map::new()));
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
                let mut raw = BTreeMap::new();
                if user_allowed {
                    fold(&mut raw, read_file(&user).await);
                }
                if project_allowed {
                    fold(&mut raw, read_file(&project).await);
                    fold(&mut raw, read_file(&local).await);
                }
                fold(&mut raw, flag);
                fold(&mut raw, managed_view(&managed));
                let env: BTreeMap<String, String> = std::env::vars().collect();
                let (env_settings, _) = lingxi_core::settings::env_parser::parse_env(&env)
                    .map_err(|error| ModError::Hook(error.to_string()))?;
                fold(
                    &mut raw,
                    serde_json::to_value(env_settings)
                        .map_err(|error| ModError::Hook(error.to_string()))?,
                );
                Ok(Value::Object(raw.into_iter().collect()))
            }
            Some(_) => unreachable!("source validated above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            managed_view(&tiers),
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
        let mut merged = BTreeMap::new();
        fold(
            &mut merged,
            json!({"enabledPlugins":{"a@org":true},"custom":{"lower":true},"model":"a"}),
        );
        fold(
            &mut merged,
            json!({"enabledPlugins":{"b@org":true},"custom":{"upper":true},"model":null}),
        );
        assert_eq!(
            Value::Object(merged.into_iter().collect()),
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
            assert_eq!(
                reader.read(json!({"source":source})).await.unwrap()["custom"],
                expected
            );
        }
        cfg.setting_source_scope = (false, false);
        let restricted = DesktopModSettingsReader::new(cfg);
        for source in ["user", "project", "local"] {
            assert_eq!(
                restricted.read(json!({"source":source})).await.unwrap(),
                json!({})
            );
        }
        assert!(restricted.read(json!({"source":"unknown"})).await.is_err());
        std::fs::write(reader.cfg.lingxi_home.join("settings.json"), "{broken").unwrap();
        assert_eq!(
            reader.read(json!({"source":"user"})).await.unwrap(),
            json!({})
        );
    }
}
