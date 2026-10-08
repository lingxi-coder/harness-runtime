//! Native effort launch state from the host's explicitly admitted config home.
use lingxi_core::host::effort_table::TableOptions;
use std::path::Path;

pub(crate) fn table_options(config_home: &Path) -> TableOptions {
    let native_legacy = config_home.join(branding::LEGACY_GLOBAL_CONFIG_FILE);
    let path = if native_legacy.exists() {
        Some(native_legacy)
    } else {
        migrations::global_config::lingxi_config_home()
            .filter(|home| home == config_home)
            .and_then(|_| migrations::global_config::global_config_path())
    };
    let global = path
        .and_then(|path| migrations::global_config::read_map(&path).ok())
        .unwrap_or_default();
    TableOptions::from_global(
        &serde_json::Value::Object(global),
        std::env::var("ANTHROPIC_DEFAULT_FABLE_MODEL").ok(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_home_metadata_controls_first_start_without_ambient_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let options = table_options(temp.path());
        assert_eq!(options.excluded_user_models.len(), 3);
        std::fs::write(
            temp.path().join(branding::LEGACY_GLOBAL_CONFIG_FILE),
            r#"{"firstStartVersion":null}"#,
        )
        .unwrap();
        assert!(table_options(temp.path()).excluded_user_models.is_empty());
        std::fs::write(
            temp.path().join(branding::LEGACY_GLOBAL_CONFIG_FILE),
            r#"{"unpinOpus47LaunchEffort":true,"unpinFable5LaunchEffort":"yes"}"#,
        )
        .unwrap();
        assert_eq!(
            table_options(temp.path()).excluded_user_models,
            ["claude-opus-4-8"]
        );
    }
}
