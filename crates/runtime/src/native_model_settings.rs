//! Capture exact admitted settings for model validation from one host snapshot.

use agent::model_resolution::{NativeModelObservation, NativeModelSettingsSnapshot};
use lingxi_core::settings::{tracer::Source, EffectiveSettings};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub(crate) fn snapshot(effective: Option<&EffectiveSettings>) -> NativeModelSettingsSnapshot {
    let opus = match std::env::var("ANTHROPIC_DEFAULT_OPUS_MODEL") {
        Ok(value) => NativeModelObservation::Observed(Some(value.into())),
        Err(std::env::VarError::NotPresent) => NativeModelObservation::Observed(None),
        Err(std::env::VarError::NotUnicode(_)) => NativeModelObservation::Unobserved,
    };
    let mut snapshot = settings_snapshot(effective);
    snapshot.anthropic_default_opus_model = opus;
    snapshot
}

fn settings_snapshot(effective: Option<&EffectiveSettings>) -> NativeModelSettingsSnapshot {
    let Some(effective) = effective
        .filter(|settings| settings.source_projection.is_some() && !settings.source_incomplete)
    else {
        return NativeModelSettingsSnapshot::default();
    };
    let layers = effective.source_field_layers("modelPicker");
    // Native dSe uses the first defined allowed local source, rather than
    // merging options or accepting project/local tiers.
    let winner = [Source::Managed, Source::Cli, Source::User]
        .iter()
        .find_map(|source| layers.iter().rev().find(|layer| layer.source == *source));
    let picker_options = match winner {
        None => NativeModelObservation::Observed(None),
        Some(layer) => picker_models(&layer.projection),
    };
    NativeModelSettingsSnapshot {
        model_overrides: NativeModelObservation::Observed(effective.source_field("modelOverrides")),
        picker_options,
        anthropic_default_opus_model: NativeModelObservation::Unobserved,
    }
}

fn picker_models(
    source: &Utf16JsonProjection,
) -> NativeModelObservation<Option<Vec<Utf16JsonProjection>>> {
    let Some(count) = source.array_len("/modelPicker/options") else {
        return NativeModelObservation::Unobserved;
    };
    let mut options = Vec::with_capacity(count);
    for index in 0..count {
        let Ok(model) = source.subprojection(&format!("/modelPicker/options/{index}/model")) else {
            return NativeModelObservation::Unobserved;
        };
        if model.string_units("").is_none() {
            return NativeModelObservation::Unobserved;
        }
        options.push(model);
    }
    NativeModelObservation::Observed(Some(options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::settings::{raw_layer_stack::SettingsRawLayer, SettingsJson};

    fn effective(layers: Vec<(Source, &str)>) -> EffectiveSettings {
        EffectiveSettings {
            settings: SettingsJson::default(),
            trace: Default::default(),
            effort_layers: Vec::new(),
            fusion_layers: Vec::new(),
            source_projection: Some(Utf16JsonProjection::parse("{}").unwrap()),
            source_layers: layers
                .into_iter()
                .map(|(source, json)| SettingsRawLayer {
                    source,
                    path: None,
                    projection: Utf16JsonProjection::parse(json).unwrap(),
                })
                .collect(),
            typed_view_unrepresented_fields: Vec::new(),
            managed_typed_view_is_partial: false,
            source_incomplete: false,
            source_message: None,
            source_diagnostics: None,
        }
    }

    #[test]
    fn picker_uses_local_native_priority_and_preserves_source_units() {
        let settings = effective(vec![
            (
                Source::User,
                r#"{"modelPicker":{"options":[{"model":"user"}]}}"#,
            ),
            (
                Source::Local,
                r#"{"modelPicker":{"options":[{"model":"local"}]}}"#,
            ),
            (
                Source::Project,
                r#"{"modelPicker":{"options":[{"model":"project"}]}}"#,
            ),
            (
                Source::Cli,
                r#"{"modelPicker":{"options":[{"model":"flag"}]}}"#,
            ),
            (
                Source::Managed,
                r#"{"modelPicker":{"options":[{"model":"\ud800"}]}}"#,
            ),
        ]);
        let NativeModelObservation::Observed(Some(options)) =
            settings_snapshot(Some(&settings)).picker_options
        else {
            panic!("complete admitted picker source must be observed");
        };
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].string_units(""), Some(vec![0xd800]));
        let project_only = effective(vec![(
            Source::Project,
            r#"{"modelPicker":{"options":[{"model":"project"}]}}"#,
        )]);
        assert_eq!(
            settings_snapshot(Some(&project_only)).picker_options,
            NativeModelObservation::Observed(None)
        );
    }

    #[test]
    fn admitted_user_loader_preserves_native_model_setting_source_units() {
        use lingxi_core::settings::{FileLayerScope, LoadInputs, RawSupplementalLayers, Settings};
        let directory = tempfile::tempdir().unwrap();
        let user_path = directory.path().join("settings.json");
        std::fs::write(&user_path,
            r#"{"modelOverrides":{"claude-opus-4-6":"\ud800"},"modelPicker":{"options":[{"model":"\ud800"}]}}"#,
        ).unwrap();
        let env = std::collections::BTreeMap::new();
        let effective = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &env,
                project_dir: directory.path(),
                defaults: SettingsJson::default(),
            },
            FileLayerScope::ALL,
            RawSupplementalLayers::default(),
            Some(&user_path),
        )
        .unwrap()
        .into_effective_settings()
        .unwrap();
        let facts = settings_snapshot(Some(&effective));
        let NativeModelObservation::Observed(Some(overrides)) = facts.model_overrides else {
            panic!("Native-valid user overrides must reach the actual host snapshot");
        };
        assert_eq!(
            overrides.string_units("/claude-opus-4-6"),
            Some(vec![0xd800])
        );
        let NativeModelObservation::Observed(Some(options)) = facts.picker_options else {
            panic!("Native-valid user picker rows must reach the actual host snapshot");
        };
        assert_eq!(options[0].string_units(""), Some(vec![0xd800]));
    }

    #[test]
    fn incomplete_settings_are_not_converted_to_observed_absence() {
        let mut settings = effective(Vec::new());
        settings.source_incomplete = true;
        assert_eq!(
            settings_snapshot(Some(&settings)).model_overrides,
            NativeModelObservation::Unobserved
        );
        assert_eq!(
            settings_snapshot(None).picker_options,
            NativeModelObservation::Unobserved
        );
        settings.source_incomplete = false;
        assert_eq!(
            settings_snapshot(Some(&settings)).model_overrides,
            NativeModelObservation::Observed(None)
        );
        settings.source_projection = Some(
            Utf16JsonProjection::parse(r#"{"modelOverrides":{"claude-opus-4-6":"\ud800"}}"#)
                .unwrap(),
        );
        let NativeModelObservation::Observed(Some(overrides)) =
            settings_snapshot(Some(&settings)).model_overrides
        else {
            panic!("exact overrides must be observed");
        };
        assert_eq!(
            overrides.string_units("/claude-opus-4-6"),
            Some(vec![0xd800])
        );
    }
}
