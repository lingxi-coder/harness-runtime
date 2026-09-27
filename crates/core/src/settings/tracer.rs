// lingxi-code/crates/core/src/settings/tracer.rs
//! Provenance recorder for `/doctor` (M6).
//!
//! `Settings::load` calls [`ProvenanceTrace::record_layer`] after merging
//! each layer; the tracer figures out which fields changed and credits
//! the right source.

use crate::settings::schema::{strategy_for, MergeStrategy, SettingsJson};
use serde::Serialize;
use std::collections::BTreeMap;

/// Where a field's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// `LINGXI_*` / `CLAUDE_CODE_*` / `CLAUDE_*` env var.
    Env,
    /// `~/.lingxi/settings.json`.
    User,
    /// `<project_dir>/.lingxi/settings.json`.
    Project,
    /// `<project_dir>/.lingxi/settings.local.json`.
    Local,
    /// Parsed CLI `--settings` / `flagSettings`.
    Cli,
    /// Enterprise managed `policySettings`.
    Managed,
    /// Built-in defaults baseline.
    Defaults,
}

/// Per-field provenance (one entry per top-level settings field).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldProvenance {
    /// Layers that contributed (in low-to-high priority order).
    pub contributors: Vec<Source>,
}

/// Map from field name → provenance.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProvenanceTrace {
    /// Per-field source list.
    pub by_field: BTreeMap<String, FieldProvenance>,
}

impl ProvenanceTrace {
    /// Record that `source` contributed to fields present in `layer`.
    ///
    /// For `ConcatDedup` fields we append; for `DeepMerge`/`Override` we overwrite.
    pub fn record_layer(&mut self, source: Source, layer: &SettingsJson) {
        for (field_name, is_present) in field_presence(layer) {
            if !is_present {
                continue;
            }
            let strat = strategy_for(field_name).unwrap_or(MergeStrategy::Override);
            let entry = self
                .by_field
                .entry(field_name.to_string())
                .or_insert_with(|| FieldProvenance {
                    contributors: Vec::new(),
                });
            match strat {
                MergeStrategy::ConcatDedup => entry.contributors.push(source),
                MergeStrategy::DeepMerge | MergeStrategy::Override => {
                    entry.contributors.clear();
                    entry.contributors.push(source);
                }
            }
        }
    }
}

/// Reflect which top-level fields are `Some(_)` in this layer.
fn field_presence(layer: &SettingsJson) -> Vec<(&'static str, bool)> {
    vec![
        ("trustedDirectories", layer.trusted_directories.is_some()),
        (
            "additionalDirectories",
            layer.additional_directories.is_some(),
        ),
        (
            "workflowSizeGuideline",
            layer.workflow_size_guideline.is_some(),
        ),
        (
            "bashEditDiffEnabled",
            layer.bash_edit_diff_enabled.is_some(),
        ),
        ("enableWorkflows", layer.enable_workflows.is_some()),
        ("enabledTools", layer.enabled_tools.is_some()),
        ("additionalIncludes", layer.additional_includes.is_some()),
        ("sandbox", layer.sandbox.is_some()),
        ("hooks", layer.hooks.is_some()),
        ("outputStyle", layer.output_style.is_some()),
        ("statusLine", layer.status_line.is_some()),
        ("subagentStatusLine", layer.subagent_status_line.is_some()),
        ("telemetryEnabled", layer.telemetry_enabled.is_some()),
        (
            "visionDelegationEnabled",
            layer.vision_delegation_enabled.is_some(),
        ),
        ("model", layer.model.is_some()),
        // 2.1.198 AWS/GCP auth-refresh script keys (scalar-override). The
        // Project provenance of awsAuthRefresh feeds the workspace-trust gate
        // (binary `mqe`: project/local-sourced command + trust unconfirmed ⇒
        // refuse to execute).
        ("awsAuthRefresh", layer.aws_auth_refresh.is_some()),
        ("awsCredentialExport", layer.aws_credential_export.is_some()),
        ("gcpAuthRefresh", layer.gcp_auth_refresh.is_some()),
        ("providers", layer.providers.is_some()),
        ("routing", layer.routing.is_some()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::schema::SettingsJson;
    use crate::settings::test_support::HOME_LOCK;
    use crate::settings::{LoadInputs, Settings};
    use std::collections::BTreeMap;
    use std::io::Write;

    #[test]
    fn scalar_override_records_winning_source() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home_t9a").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        let project_subdir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();

        let mut uf = std::fs::File::create(user_dir.join("settings.json")).unwrap();
        writeln!(uf, r#"{{"model": "user-model"}}"#).unwrap();
        std::env::set_var("HOME", tmp.path().join("home_t9a"));

        let eff = Settings::load(LoadInputs {
            env: &BTreeMap::new(),
            project_dir,
            defaults: SettingsJson::default(),
        })
        .unwrap();

        let prov = eff
            .effective_for("model")
            .expect("model must have provenance");
        assert_eq!(prov.contributors, vec![Source::User]);
    }

    #[test]
    fn concat_dedup_records_all_file_contributors_in_order() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home_t9b").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        let project_subdir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();
        let local_path = project_subdir.join("settings.local.json");

        let mut pf = std::fs::File::create(project_subdir.join("settings.json")).unwrap();
        writeln!(pf, r#"{{"trustedDirectories": ["/project"]}}"#).unwrap();
        let mut uf = std::fs::File::create(user_dir.join("settings.json")).unwrap();
        writeln!(uf, r#"{{"trustedDirectories": ["/user"]}}"#).unwrap();
        let mut lf = std::fs::File::create(local_path).unwrap();
        writeln!(lf, r#"{{"trustedDirectories": ["/local"]}}"#).unwrap();
        std::env::set_var("HOME", tmp.path().join("home_t9b"));

        let defaults = SettingsJson {
            trusted_directories: Some(vec!["/default".into()]),
            ..Default::default()
        };
        let eff = Settings::load(LoadInputs {
            env: &BTreeMap::new(),
            project_dir,
            defaults,
        })
        .unwrap();
        let prov = eff.effective_for("trustedDirectories").unwrap();
        assert_eq!(
            prov.contributors,
            vec![
                Source::Defaults,
                Source::User,
                Source::Project,
                Source::Local
            ]
        );
    }

    #[test]
    fn unknown_field_returns_none() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path().join("no_home"));
        let eff = Settings::load(LoadInputs {
            env: &BTreeMap::new(),
            project_dir: tmp.path(),
            defaults: SettingsJson::default(),
        })
        .unwrap();
        assert!(eff.effective_for("nonsense").is_none());
    }

    #[test]
    fn enable_workflows_provenance_is_recorded() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home_t9c").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();

        let mut uf = std::fs::File::create(user_dir.join("settings.json")).unwrap();
        writeln!(uf, r#"{{"enableWorkflows": false}}"#).unwrap();
        std::env::set_var("HOME", tmp.path().join("home_t9c"));

        let defaults = SettingsJson {
            enable_workflows: Some(true),
            ..Default::default()
        };
        let eff = Settings::load(LoadInputs {
            env: &BTreeMap::new(),
            project_dir,
            defaults,
        })
        .unwrap();

        let prov = eff
            .effective_for("enableWorkflows")
            .expect("enableWorkflows must have provenance");
        assert_eq!(prov.contributors, vec![Source::User]);
    }
}
