//! Source-preserving views of the effective settings tier stack.
//!
//! This is used by host projections that need the exact merged JSON values
//! alongside the source tier that contributed them. The all-source merge is an
//! evidence view; the admitted merge contains ordinary raw projections that
//! passed typed validation plus the Native-selected normalized managed-policy
//! projection. Both merges avoid routing source JSON through `serde_json::Value`.

use crate::settings::loader;
use crate::settings::merger;
use crate::settings::native_policy::NativePolicySelection;
use crate::settings::tracer;
use crate::settings::{
    env_parser, FileLayerScope, LoadInputs, Settings, SettingsError, SettingsJson,
};
use crate::types::utf16_json::Utf16JsonProjection;
use std::path::{Path, PathBuf};

/// One syntactically valid source tier and its value before raw merges.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsRawLayer {
    /// Source identity used by native policy and provenance checks.
    pub source: tracer::Source,
    /// File path for disk-backed settings; absent for synthesized layers.
    pub path: Option<PathBuf>,
    /// Exact JSON value for this layer, including UTF-16 string code units.
    pub projection: Utf16JsonProjection,
}

/// Ordinary source layers plus the Native-selected managed projection.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsRawLayerStack {
    /// Source objects in ascending merge-priority order. Ordinary file/CLI
    /// tiers remain original evidence; the managed entry is the normalized
    /// composed policy projection, whose original documents are retained in
    /// `managed_policy_selection`.
    pub layers: Vec<SettingsRawLayer>,
    /// Merged source evidence after ordinary syntactically valid tiers and the
    /// selected managed policy, including ordinary tiers rejected by typed
    /// Native settings validation.
    pub merged: Utf16JsonProjection,
    /// Raw source projections admitted by their owning parser. Ordinary tiers
    /// pass typed validation; the managed-policy projection comes from Native
    /// policy selection.
    pub admitted_raw_layers: Vec<SettingsRawLayer>,
    /// Layers accepted by the current typed settings parser and validation.
    /// This is the source-aware effective view for consumers that need both
    /// Native admission and per-tier values.
    pub admitted_layers: Vec<SettingsAdmittedLayer>,
    /// The exact policy documents, source ordering, and diagnostics used to
    /// produce the composed managed layer. Kept even when the policy read is
    /// incomplete or has no effective projection.
    pub managed_policy_selection: Option<NativePolicySelection>,
    admitted_merged: Utf16JsonProjection,
}

/// A source layer that also passes the current typed Native settings schema.
///
/// [`SettingsRawLayerStack::layers`] remains the lossless evidence view and
/// includes syntactically valid object layers even when a known setting makes
/// Native reject the entire layer. Consumers that need effective typed values
/// must use this view rather than infer admission from the raw merged object.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsAdmittedLayer {
    /// Source identity used by native policy and provenance checks.
    pub source: tracer::Source,
    /// File path for disk-backed settings; absent for synthesized layers.
    pub path: Option<PathBuf>,
    /// Typed convenience view for this source tier. Native policy projections
    /// can be partial when valid JavaScript UTF-16 values cannot fit Rust
    /// `String`; consult the exact source projection before treating a missing
    /// typed value as absent.
    pub settings: SettingsJson,
    /// Managed fields retained in the authoritative source but not represented
    /// in this typed convenience view.
    pub typed_view_unrepresented_fields: Vec<String>,
    /// This layer was admitted by Native's policy parser rather than by the
    /// ordinary all-fields `SettingsJson` parser.
    pub native_policy_projection: bool,
}

/// Raw non-file settings sources, ordered exactly as their merge precedence.
/// Invalid CLI JSON or a non-object CLI tier is skipped. Managed policy uses
/// its separate Native parser and source selector rather than generic typed
/// SettingsJson admission.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawSupplementalLayers<'a> {
    /// Original CLI `--settings` / `flagSettings` JSON text.
    pub cli_layer: Option<&'a str>,
    /// Managed `policySettings` selected and normalized by the Native policy
    /// parser and source selector. The original documents remain attached to
    /// the selection for diagnostics and provenance.
    pub managed_policy: Option<&'a NativePolicySelection>,
}

impl Settings {
    /// Load the source-preserving defaults, user, project, local, CLI, managed,
    /// and environment settings stack.
    ///
    /// File scope, paths, and file size/regular-file checks match
    /// [`Settings::load_with_layers_from_user_path`]. `supplemental` carries
    /// raw CLI JSON and the source-aware Native policy selection so consumers
    /// do not reconstruct policy from typed settings and lose passthrough
    /// values, source order, or exact UTF-16 units.
    /// This source view intentionally retains schema-invalid layers as evidence;
    /// use [`SettingsRawLayerStack::admitted_typed_layers`] before consuming a
    /// layer as effective settings.
    pub fn load_raw_layer_stack_from_user_path(
        inputs: LoadInputs<'_>,
        file_scope: FileLayerScope,
        supplemental: RawSupplementalLayers<'_>,
        user_settings_path: Option<&Path>,
    ) -> Result<SettingsRawLayerStack, SettingsError> {
        let LoadInputs {
            env,
            project_dir,
            defaults,
        } = inputs;
        let mut layers = Vec::new();
        let mut admitted_raw_layers = Vec::new();
        let mut admitted_layers = Vec::new();

        record_layer(
            &mut layers,
            &mut admitted_raw_layers,
            &mut admitted_layers,
            SettingsRawLayer {
                source: tracer::Source::Defaults,
                path: None,
                projection: projection_from_settings(&defaults)?,
            },
            Some(defaults.clone()),
        );

        if file_scope.include_user {
            if let Some(path) = user_settings_path {
                if let Some(snapshot) = read_file_layer_or_skip(path) {
                    record_layer(
                        &mut layers,
                        &mut admitted_raw_layers,
                        &mut admitted_layers,
                        SettingsRawLayer {
                            source: tracer::Source::User,
                            path: Some(path.to_path_buf()),
                            projection: snapshot.projection,
                        },
                        snapshot.admitted,
                    );
                }
            }
        }

        if file_scope.include_project {
            let path = loader::project_settings_path(project_dir);
            if let Some(snapshot) = read_file_layer_or_skip(&path) {
                record_layer(
                    &mut layers,
                    &mut admitted_raw_layers,
                    &mut admitted_layers,
                    SettingsRawLayer {
                        source: tracer::Source::Project,
                        path: Some(path),
                        projection: snapshot.projection,
                    },
                    snapshot.admitted,
                );
            }
        }

        if file_scope.include_local {
            let path = loader::local_settings_path(project_dir);
            if let Some(snapshot) = read_file_layer_or_skip(&path) {
                record_layer(
                    &mut layers,
                    &mut admitted_raw_layers,
                    &mut admitted_layers,
                    SettingsRawLayer {
                        source: tracer::Source::Local,
                        path: Some(path),
                        projection: snapshot.projection,
                    },
                    snapshot.admitted,
                );
            }
        }

        if let Some(raw) = supplemental.cli_layer {
            if let Some((projection, admitted)) = parse_supplemental_layer(raw) {
                record_layer(
                    &mut layers,
                    &mut admitted_raw_layers,
                    &mut admitted_layers,
                    SettingsRawLayer {
                        source: tracer::Source::Cli,
                        path: None,
                        projection,
                    },
                    admitted,
                );
            }
        }

        if let Some(selection) = supplemental.managed_policy {
            if let Some(effective) = selection.effective.as_ref() {
                record_native_policy_layer(
                    &mut layers,
                    &mut admitted_raw_layers,
                    &mut admitted_layers,
                    SettingsRawLayer {
                        source: tracer::Source::Managed,
                        path: None,
                        projection: effective.projection.clone(),
                    },
                    effective.settings.clone(),
                );
            }
        }

        let (env_layer, _invalid_env) = env_parser::parse_env(env)?;
        record_layer(
            &mut layers,
            &mut admitted_raw_layers,
            &mut admitted_layers,
            SettingsRawLayer {
                source: tracer::Source::Env,
                path: None,
                projection: projection_from_settings(&env_layer)?,
            },
            Some(env_layer),
        );

        let mut merged = Utf16JsonProjection::parse("{}")?;
        for layer in &layers {
            merger::merge_raw_source_layer(&mut merged, &layer.projection)?;
        }
        let mut admitted_merged = Utf16JsonProjection::parse("{}")?;
        for layer in &admitted_raw_layers {
            merger::merge_raw_source_layer(&mut admitted_merged, &layer.projection)?;
        }

        Ok(SettingsRawLayerStack {
            layers,
            merged,
            admitted_raw_layers,
            admitted_layers,
            managed_policy_selection: supplemental.managed_policy.cloned(),
            admitted_merged,
        })
    }
}

impl SettingsRawLayerStack {
    /// Return the raw JSON merged from ordinary tiers that passed typed
    /// settings validation plus the Native-selected normalized managed policy.
    ///
    /// This keeps unknown/passthrough properties and exact UTF-16 units from
    /// accepted source snapshots while excluding ordinary schema-rejected
    /// tiers. The managed-policy source selection remains available through
    /// [`Self::managed_policy_selection`]. [`Self::merged`] remains the
    /// all-source evidence view. Check the retained selection's `complete`
    /// field before treating the combined view as fully resolved.
    #[must_use]
    pub fn admitted_merged(&self) -> &Utf16JsonProjection {
        &self.admitted_merged
    }

    /// Build an effective convenience view from this exact source snapshot.
    /// Source projections remain authoritative and include fields which cannot
    /// be represented by `SettingsJson`.
    pub fn into_effective_settings(
        &self,
    ) -> Result<crate::settings::EffectiveSettings, SettingsError> {
        let mut settings = SettingsJson::default();
        let mut trace = tracer::ProvenanceTrace::default();
        let mut effort_layers = Vec::new();
        let mut typed_view_unrepresented_fields = Vec::new();
        let mut managed_typed_view_is_partial = false;

        for layer in &self.admitted_layers {
            trace.record_layer(layer.source, &layer.settings);
            for path in &layer.typed_view_unrepresented_fields {
                let Some(segment) = path.strip_prefix('/') else {
                    continue;
                };
                let top_level = segment
                    .split('/')
                    .next()
                    .unwrap_or(segment)
                    .replace("~1", "/")
                    .replace("~0", "~");
                if top_level.is_empty() || top_level.starts_with('<') {
                    continue;
                }
                let source = trace.by_field.entry(top_level.clone()).or_insert_with(|| {
                    tracer::FieldProvenance {
                        contributors: Vec::new(),
                    }
                });
                match crate::settings::schema::strategy_for(&top_level)
                    .unwrap_or(crate::settings::schema::MergeStrategy::Override)
                {
                    crate::settings::schema::MergeStrategy::ConcatDedup
                    | crate::settings::schema::MergeStrategy::DeepMerge => {
                        if !source.contributors.contains(&layer.source) {
                            source.contributors.push(layer.source);
                        }
                    }
                    crate::settings::schema::MergeStrategy::Override => {
                        source.contributors.clear();
                        source.contributors.push(layer.source);
                    }
                }
            }
            effort_layers.push(crate::host::effort::EffortSettingsLayer::new(
                layer.source,
                &layer.settings,
            ));
            settings = merger::merge(settings, layer.settings.clone())?;
            if layer.source == tracer::Source::Managed {
                managed_typed_view_is_partial = layer.native_policy_projection;
            }
            typed_view_unrepresented_fields
                .extend(layer.typed_view_unrepresented_fields.iter().cloned());
        }

        Ok(crate::settings::EffectiveSettings {
            settings,
            trace,
            effort_layers,
            source_projection: Some(self.admitted_merged.clone()),
            source_layers: self.admitted_raw_layers.clone(),
            typed_view_unrepresented_fields,
            managed_typed_view_is_partial,
            source_incomplete: self
                .managed_policy_selection
                .as_ref()
                .is_some_and(|selection| !selection.complete),
            source_message: None,
            source_diagnostics: None,
        })
    }

    /// Return the source tiers with an optional typed Core view.
    ///
    /// Ordinary tiers mirror Native's per-file `safeParse` behavior: a known
    /// invalid field rejects the whole tier, while raw evidence remains in
    /// [`Self::layers`] and [`Self::merged`]. The managed policy was selected
    /// by the dedicated Native parser; its typed Core view is included only
    /// when representable, while its normalized raw projection is retained
    /// independently. Inspect [`Self::managed_policy_selection`] for source
    /// completeness and unresolved fields.
    #[must_use]
    pub fn admitted_typed_layers(&self) -> Vec<SettingsAdmittedLayer> {
        self.admitted_layers.clone()
    }
}

fn record_layer(
    layers: &mut Vec<SettingsRawLayer>,
    admitted_raw_layers: &mut Vec<SettingsRawLayer>,
    admitted_layers: &mut Vec<SettingsAdmittedLayer>,
    layer: SettingsRawLayer,
    admitted: Option<SettingsJson>,
) {
    let (settings, typed_view_unrepresented_fields) = match admitted {
        Some(settings) => (settings, Vec::new()),
        None => match partial_typed_view_for_unpaired_utf16(&layer.projection) {
            Some(partial) => partial,
            None => {
                layers.push(layer);
                return;
            }
        },
    };
    {
        admitted_raw_layers.push(layer.clone());
        admitted_layers.push(SettingsAdmittedLayer {
            source: layer.source,
            path: layer.path.clone(),
            settings,
            typed_view_unrepresented_fields,
            native_policy_projection: false,
        });
    }
    layers.push(layer);
}

/// Preserve a narrowly representable ordinary layer when serde rejected it
/// only because a JSON string contains an unpaired UTF-16 code unit. The
/// original source remains untouched; only those strings (or array fields
/// whose element positions cannot be represented) are omitted from the typed
/// convenience view. Every remaining field still passes the existing
/// whole-`SettingsJson` parse and validation, so an unrelated schema error
/// rejects the entire ordinary layer as before.
fn partial_typed_view_for_unpaired_utf16(
    source: &Utf16JsonProjection,
) -> Option<(SettingsJson, Vec<String>)> {
    let mut unrepresented = Vec::new();
    let cleaned = strip_only_unpaired_utf16_strings(source, "", &mut unrepresented)?;
    if unrepresented.is_empty() {
        return None;
    }
    let json = cleaned.to_json_string().ok()?;
    let settings = serde_json::from_str::<SettingsJson>(&json).ok()?;
    settings.validate().ok()?;
    Some((settings, unrepresented))
}

fn strip_only_unpaired_utf16_strings(
    source: &Utf16JsonProjection,
    pointer: &str,
    unrepresented: &mut Vec<String>,
) -> Option<Utf16JsonProjection> {
    let value = source.at_pointer("")?;
    if let Some(units) = value.code_units() {
        if String::from_utf16(units).is_ok() {
            return Some(source.clone());
        }
        unrepresented.push(pointer.to_owned());
        return None;
    }
    if value.is_null() || value.as_bool().is_some() || value.as_number().is_some() {
        return Some(source.clone());
    }
    if let Some((logical_length, entries)) = value.array_entries() {
        if entries.len() != logical_length as usize {
            unrepresented.push(pointer.to_owned());
            return None;
        }
        let mut values = Vec::with_capacity(entries.len());
        let mut changed = false;
        for (index, _) in entries {
            let child_pointer = format!("{pointer}/{index}");
            let child = source.subprojection(&format!("/{index}")).ok()?;
            let Some(child) =
                strip_only_unpaired_utf16_strings(&child, &child_pointer, unrepresented)
            else {
                // Dropping an element would shift all later indexes, so omit
                // the entire array from the typed view and keep its exact
                // source projection alongside it.
                unrepresented.push(pointer.to_owned());
                return None;
            };
            changed |= child != source.subprojection(&format!("/{index}")).ok()?;
            values.push(child);
        }
        return if changed {
            Utf16JsonProjection::array(values).ok()
        } else {
            Some(source.clone())
        };
    }
    if let Some(entries) = value.object_entries() {
        let mut fields = Vec::with_capacity(entries.len());
        let mut changed = false;
        for entry in entries {
            let key = entry.code_units.to_vec();
            let Ok(key_text) = String::from_utf16(&key) else {
                unrepresented.push(format!("{pointer}/<unpaired-key>"));
                changed = true;
                continue;
            };
            let child_pointer = format!(
                "{pointer}/{}",
                key_text.replace('~', "~0").replace('/', "~1")
            );
            let original = source.subprojection_property_units(&key).ok()?;
            if let Some(child) =
                strip_only_unpaired_utf16_strings(&original, &child_pointer, unrepresented)
            {
                changed |= child != original;
                fields.push((key, child));
            } else {
                changed = true;
            }
        }
        return if changed {
            Utf16JsonProjection::object_units(fields).ok()
        } else {
            Some(source.clone())
        };
    }
    Some(source.clone())
}

fn record_native_policy_layer(
    layers: &mut Vec<SettingsRawLayer>,
    admitted_raw_layers: &mut Vec<SettingsRawLayer>,
    admitted_layers: &mut Vec<SettingsAdmittedLayer>,
    layer: SettingsRawLayer,
    settings: Option<SettingsJson>,
) {
    // Native policy selection already admitted and normalized this composed
    // source. Keep the projection even when the optional typed Core view cannot
    // represent a setting field; raw-policy consumers still need the source.
    admitted_raw_layers.push(layer.clone());
    let (settings, typed_view_unrepresented_fields) = settings.map_or_else(
        || parse_native_policy_typed_view(&layer.projection),
        |settings| (settings, Vec::new()),
    );
    admitted_layers.push(SettingsAdmittedLayer {
        source: layer.source,
        path: layer.path.clone(),
        settings,
        typed_view_unrepresented_fields,
        native_policy_projection: true,
    });
    layers.push(layer);
}

fn parse_native_policy_typed_view(source: &Utf16JsonProjection) -> (SettingsJson, Vec<String>) {
    let Some(entries) = source.object_entries() else {
        return (SettingsJson::default(), vec!["<non-object-root>".into()]);
    };

    let mut representable = Vec::new();
    let mut unrepresented = Vec::new();
    for entry in entries {
        let field = String::from_utf16(entry.code_units)
            .unwrap_or_else(|_| format!("<key-units:{:x?}>", entry.code_units));
        let Some(original) = source.subprojection_property_units(entry.code_units).ok() else {
            unrepresented.push(format!("/{field}"));
            continue;
        };
        let Ok(original_field) =
            Utf16JsonProjection::object_units(vec![(entry.code_units.to_vec(), original.clone())])
        else {
            unrepresented.push(format!("/{field}"));
            continue;
        };
        let Ok(original_json) = original_field.to_json_string() else {
            unrepresented.push(format!("/{field}"));
            continue;
        };
        if serde_json::from_str::<SettingsJson>(&original_json).is_ok() {
            representable.push((entry.code_units.to_vec(), original));
            continue;
        }

        let Some(value) =
            strip_unrepresentable_strings(&original, &format!("/{field}"), &mut unrepresented)
        else {
            unrepresented.push(format!("/{field}"));
            continue;
        };
        let Ok(single_field) =
            Utf16JsonProjection::object_units(vec![(entry.code_units.to_vec(), value.clone())])
        else {
            unrepresented.push(format!("/{field}"));
            continue;
        };
        let Ok(json) = single_field.to_json_string() else {
            unrepresented.push(format!("/{field}"));
            continue;
        };
        if serde_json::from_str::<SettingsJson>(&json).is_ok() {
            representable.push((entry.code_units.to_vec(), value));
        } else {
            unrepresented.push(format!("/{field}"));
        }
    }

    let typed_projection = Utf16JsonProjection::object_units(representable)
        .unwrap_or_else(|_| Utf16JsonProjection::parse("{}").expect("empty JSON object"));
    let settings = typed_projection
        .to_json_string()
        .ok()
        .and_then(|json| serde_json::from_str::<SettingsJson>(&json).ok())
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    unrepresented.retain(|field| seen.insert(field.clone()));
    (settings, unrepresented)
}

/// Make a typed convenience subtree from the subset that can cross serde's
/// UTF-8 `String` boundary. The exact managed source remains untouched in the
/// adjacent `SettingsRawLayer`; this function only prevents one unpaired
/// sibling string from erasing other representable settings fields.
fn strip_unrepresentable_strings(
    source: &Utf16JsonProjection,
    pointer: &str,
    unrepresented: &mut Vec<String>,
) -> Option<Utf16JsonProjection> {
    let value = source.at_pointer("")?;
    if let Some(units) = value.code_units() {
        if String::from_utf16(units).is_ok() {
            return Some(source.clone());
        }
        unrepresented.push(pointer.to_owned());
        return None;
    }
    if value.is_null() || value.as_bool().is_some() || value.as_number().is_some() {
        return Some(source.clone());
    }
    if let Some((logical_length, entries)) = value.array_entries() {
        // A serde vector cannot represent Native `undefined` holes. Do not
        // densify sparse arrays (their logical length can be very large); keep
        // them solely in the exact source projection.
        if entries.len() != logical_length as usize {
            unrepresented.push(pointer.to_owned());
            return None;
        }
        let mut retained = Vec::with_capacity(entries.len());
        let mut changed = false;
        for (index, _) in entries {
            let child_pointer = format!("{pointer}/{index}");
            let child_source = source.subprojection(&format!("/{index}")).ok()?;
            if let Some(child) =
                strip_unrepresentable_strings(&child_source, &child_pointer, unrepresented)
            {
                changed |= child != child_source;
                retained.push((index, child));
            } else {
                changed = true;
            }
        }
        if !changed {
            return Some(source.clone());
        }
        // Dropping an unrepresentable array member would shift following
        // string indexes in a dense typed vector. Keep the entire array out of
        // the typed convenience view instead of silently changing the rule.
        unrepresented.push(pointer.to_owned());
        return None;
    }
    if let Some(entries) = value.object_entries() {
        let mut retained = Vec::with_capacity(entries.len());
        let mut changed = false;
        for entry in entries {
            let Ok(key) = String::from_utf16(entry.code_units) else {
                unrepresented.push(format!("{pointer}/<unrepresentable-key>"));
                changed = true;
                continue;
            };
            let child_pointer = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
            let child_source = source.subprojection_property_units(entry.code_units).ok()?;
            if let Some(child) =
                strip_unrepresentable_strings(&child_source, &child_pointer, unrepresented)
            {
                changed |= child != child_source;
                retained.push((entry.code_units.to_vec(), child));
            } else {
                changed = true;
            }
        }
        if !changed {
            return Some(source.clone());
        }
        return Utf16JsonProjection::object_units(retained).ok();
    }
    unrepresented.push(pointer.to_owned());
    None
}

fn read_file_layer_or_skip(path: &Path) -> Option<loader::SettingsLayerSnapshot> {
    match loader::read_settings_layer_snapshot(path) {
        Ok(Some(snapshot)) => Some(snapshot),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed to read raw settings layer; skipping it"
            );
            None
        }
    }
}

fn projection_from_settings(settings: &SettingsJson) -> Result<Utf16JsonProjection, SettingsError> {
    let source = serde_json::to_string(settings)
        .map_err(|error| SettingsError::SchemaViolation(error.to_string()))?;
    Ok(Utf16JsonProjection::parse(&source)?)
}

fn parse_supplemental_layer(raw: &str) -> Option<(Utf16JsonProjection, Option<SettingsJson>)> {
    let projection = Utf16JsonProjection::parse(raw).ok()?;
    projection.object_entries()?;
    let admitted = serde_json::from_str::<SettingsJson>(raw)
        .ok()
        .filter(|settings| settings.validate().is_ok());
    Some((projection, admitted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::native_policy::{
        resolve_native_policy, NativePolicyDocument, NativePolicyInputs, NativePolicyLoadState,
        NativePolicyManagedFileFacts,
    };
    use crate::settings::tracer::Source;
    use std::collections::BTreeMap;

    fn managed_policy(raw: &str) -> NativePolicySelection {
        let document = NativePolicyDocument::parse_managed_file(
            Some(Utf16JsonProjection::parse(raw).expect("managed policy JSON")),
            NativePolicyManagedFileFacts {
                path: PathBuf::from("/etc/claude-code/managed-settings.json"),
                load_state: NativePolicyLoadState::Loaded,
                document_has_policy_content: None,
            },
        );
        resolve_native_policy(NativePolicyInputs {
            managed_file_documents: vec![document],
            ..NativePolicyInputs::default()
        })
    }

    #[test]
    fn admitted_model_with_unpaired_utf16_keeps_native_source_and_typed_siblings() {
        let selection = managed_policy(
            r#"{"model":"\ud800","askUserQuestionTimeout":"60s","permissions":{"allow":["Read(\ud800)"],"deny":["Bash"]}}"#,
        );
        assert!(selection.complete);
        let native = selection.effective.as_ref().expect("managed projection");
        assert!(native.settings.is_none());
        assert_eq!(native.projection.string_units("/model"), Some(vec![0xd800]));

        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: Path::new("/source-authoritative-settings-test"),
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: false,
                include_project: false,
                include_local: false,
            },
            RawSupplementalLayers {
                cli_layer: None,
                managed_policy: Some(&selection),
            },
            None,
        )
        .expect("Native-admitted projection remains in the stack");

        let managed = stack
            .admitted_typed_layers()
            .into_iter()
            .find(|layer| layer.source == Source::Managed)
            .expect("a partial managed convenience view is available");
        assert_eq!(managed.settings.model, None);
        assert_eq!(
            managed.settings.ask_user_question_timeout.as_deref(),
            Some("60s")
        );
        assert_eq!(
            managed
                .settings
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.get("deny")),
            Some(&serde_json::json!(["Bash"]))
        );
        assert!(managed
            .typed_view_unrepresented_fields
            .contains(&"/model".to_owned()));
        assert!(managed
            .typed_view_unrepresented_fields
            .contains(&"/permissions/allow".to_owned()));

        let effective = stack
            .into_effective_settings()
            .expect("ordinary layers and Native source compose independently");
        assert_eq!(
            effective.settings.ask_user_question_timeout.as_deref(),
            Some("60s")
        );
        assert_eq!(
            effective
                .source_field("model")
                .as_ref()
                .and_then(|model| model.string_units("")),
            Some(vec![0xd800])
        );
        assert_eq!(
            effective.source_field_contributors("model"),
            [Source::Managed]
        );
        assert!(effective
            .typed_view_unrepresented_fields
            .contains(&"/model".to_owned()));
    }

    #[test]
    fn ordinary_layer_with_only_unpaired_strings_keeps_valid_typed_siblings() {
        let root = tempfile::tempdir().unwrap();
        let settings_path = root.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"model":"\ud800","language":"English","permissions":{"allow":["Read(\ud800)"],"deny":["Bash"]}}"#,
        )
        .unwrap();

        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: root.path(),
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: true,
                include_project: false,
                include_local: false,
            },
            RawSupplementalLayers::default(),
            Some(&settings_path),
        )
        .expect("valid source JSON remains loadable");

        let admitted = stack
            .admitted_typed_layers()
            .into_iter()
            .find(|layer| layer.source == Source::User)
            .expect("only the unpaired strings are absent from the typed view");
        assert_eq!(admitted.settings.model, None);
        assert_eq!(
            admitted
                .settings
                .language
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("English")
        );
        assert_eq!(
            admitted
                .settings
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.get("deny")),
            Some(&serde_json::json!(["Bash"]))
        );
        assert!(admitted
            .typed_view_unrepresented_fields
            .contains(&"/model".to_owned()));
        assert!(admitted
            .typed_view_unrepresented_fields
            .contains(&"/permissions/allow".to_owned()));
        assert_eq!(
            stack.admitted_merged().string_units("/model"),
            Some(vec![0xd800])
        );

        let invalid_path = root.path().join("invalid-settings.json");
        std::fs::write(
            &invalid_path,
            r#"{"model":"\ud800","askUserQuestionTimeout":4}"#,
        )
        .unwrap();
        let invalid = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: root.path(),
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: true,
                include_project: false,
                include_local: false,
            },
            RawSupplementalLayers::default(),
            Some(&invalid_path),
        )
        .expect("schema-invalid source remains evidence");
        assert!(!invalid
            .admitted_typed_layers()
            .iter()
            .any(|layer| layer.source == Source::User));
        assert!(invalid.layers.iter().any(|layer| {
            layer.source == Source::User
                && layer.projection.string_units("/model") == Some(vec![0xd800])
        }));
    }

    #[test]
    fn raw_stack_preserves_language_units_and_native_tier_precedence() {
        let root = tempfile::tempdir().unwrap();
        let user_home = root.path().join("user");
        let user_dir = user_home.join(".lingxi");
        let project_dir = root.path().join("project");
        let project_settings_dir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&project_settings_dir).unwrap();

        let user_path = user_dir.join("settings.json");
        std::fs::write(
            &user_path,
            r#"{"language":"\ud800","outputStyle":"style-\ud800","proxyAuthHelper":"/tmp/helper-\ud800","model":"raw-sibling","sandbox":{"user":true,"filesystem":{"allowWrite":["/tmp/\ud800"],"ignoreViolations":["\udfff"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            project_settings_dir.join("settings.json"),
            r#"{"language":"project","sandbox":{"project":true}}"#,
        )
        .unwrap();
        std::fs::write(
            project_settings_dir.join("settings.local.json"),
            r#"{"language":"local","sandbox":{"local":true}}"#,
        )
        .unwrap();

        let managed = managed_policy(r#"{"language":"managed","sandbox":{"managed":true}}"#);
        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: &project_dir,
                defaults: SettingsJson::default(),
            },
            FileLayerScope::ALL,
            RawSupplementalLayers {
                cli_layer: Some(r#"{"language":"cli","sandbox":{"cli":true}}"#),
                managed_policy: Some(&managed),
            },
            Some(&user_path),
        )
        .expect("valid raw settings tiers");

        assert_eq!(
            stack
                .layers
                .iter()
                .map(|layer| layer.source)
                .collect::<Vec<_>>(),
            vec![
                Source::Defaults,
                Source::User,
                Source::Project,
                Source::Local,
                Source::Cli,
                Source::Managed,
                Source::Env,
            ]
        );
        assert_eq!(stack.layers[1].path.as_deref(), Some(user_path.as_path()));
        assert_eq!(
            stack.merged.string_units("/language"),
            Some("managed".encode_utf16().collect())
        );
        assert_eq!(
            stack.merged.string_units("/outputStyle"),
            Some("style-".encode_utf16().chain([0xd800]).collect()),
            "the exact outputStyle setting remains available after typed layer merge"
        );
        assert_eq!(
            stack.layers[1].source,
            Source::User,
            "the source carrier retains the tier that supplied proxyAuthHelper"
        );
        let helper_units = "/tmp/helper-"
            .encode_utf16()
            .chain([0xd800])
            .collect::<Vec<_>>();
        assert_eq!(
            stack.layers[1].projection.string_units("/proxyAuthHelper"),
            Some(helper_units.clone())
        );
        assert_eq!(
            stack.merged.string_units("/proxyAuthHelper"),
            Some(helper_units)
        );
        assert_eq!(stack.merged.string_at("/model"), Some("raw-sibling"));
        assert_eq!(
            stack.layers[1]
                .projection
                .string_units("/sandbox/filesystem/allowWrite/0"),
            Some(vec![
                '/' as u16, 't' as u16, 'm' as u16, 'p' as u16, '/' as u16, 0xd800,
            ])
        );
        assert_eq!(
            stack
                .merged
                .string_units("/sandbox/filesystem/ignoreViolations/0"),
            Some(vec![0xdfff])
        );
        for key in ["user", "project", "local", "cli", "managed"] {
            assert!(stack
                .merged
                .at_pointer(&format!("/sandbox/{key}"))
                .is_some());
        }

        let without_user = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: &project_dir,
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: false,
                include_project: true,
                include_local: true,
            },
            RawSupplementalLayers::default(),
            Some(&user_path),
        )
        .unwrap();
        assert!(!without_user
            .layers
            .iter()
            .any(|layer| layer.source == Source::User));
        assert_eq!(without_user.merged.string_at("/language"), Some("local"));
    }

    #[test]
    fn raw_tiers_keep_json_values_independent_of_typed_execution_parsing() {
        let root = tempfile::tempdir().unwrap();
        let project_dir = root.path();
        let managed = managed_policy(r#"{"language":"accepted"}"#);
        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir,
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: false,
                include_project: false,
                include_local: false,
            },
            RawSupplementalLayers {
                cli_layer: Some(
                    r#"{"language":"cli","sandbox":{"filesystem":{"allowWrite":["/tmp/\ud800"]}}}"#,
                ),
                managed_policy: Some(&managed),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            stack
                .layers
                .iter()
                .map(|layer| layer.source)
                .collect::<Vec<_>>(),
            vec![Source::Defaults, Source::Cli, Source::Managed, Source::Env]
        );
        assert_eq!(stack.merged.string_at("/language"), Some("accepted"));
        assert_eq!(
            stack
                .merged
                .string_units("/sandbox/filesystem/allowWrite/0"),
            Some(vec![
                '/' as u16, 't' as u16, 'm' as u16, 'p' as u16, '/' as u16, 0xd800,
            ])
        );
    }

    #[test]
    fn native_managed_projection_keeps_exact_units_with_an_admitted_typed_view() {
        let root = tempfile::tempdir().unwrap();
        let policy = managed_policy(
            r#"{"availableModels":["allowed-\ud800"],"enabledPlugins":{"plugin@market":["1.0.0"]},"model":"managed-model"}"#,
        );
        // Native accepts plugin-version arrays. Admission of a convenience
        // typed view must not replace the authoritative source projection.
        assert!(policy.complete);

        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: root.path(),
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: false,
                include_project: false,
                include_local: false,
            },
            RawSupplementalLayers {
                cli_layer: None,
                managed_policy: Some(&policy),
            },
            None,
        )
        .expect("ordinary layers load independently of managed policy typing");

        let managed_layer = stack
            .admitted_raw_layers
            .iter()
            .find(|layer| layer.source == Source::Managed)
            .expect("Native-admitted managed projection remains in the raw merge");
        assert!(managed_layer.path.is_none());
        assert_eq!(
            managed_layer.projection.string_units("/availableModels/0"),
            Some("allowed-".encode_utf16().chain([0xd800]).collect())
        );
        assert!(stack
            .admitted_typed_layers()
            .iter()
            .any(|layer| layer.source == Source::Managed));
        assert!(stack
            .managed_policy_selection
            .as_ref()
            .is_some_and(|selection| selection.complete));
        assert_eq!(
            stack.admitted_merged().string_units("/availableModels/0"),
            Some("allowed-".encode_utf16().chain([0xd800]).collect())
        );
    }

    #[test]
    fn managed_policy_read_failure_is_retained_in_the_stack() {
        let root = tempfile::tempdir().unwrap();
        let unavailable = NativePolicyDocument::parse_managed_file(
            None,
            NativePolicyManagedFileFacts {
                path: PathBuf::from("/etc/claude-code/managed-settings.json"),
                load_state: NativePolicyLoadState::DidNotLoad,
                document_has_policy_content: None,
            },
        );
        let policy = resolve_native_policy(NativePolicyInputs {
            managed_file_documents: vec![unavailable],
            ..NativePolicyInputs::default()
        });
        assert!(!policy.complete);
        assert!(policy.effective.is_none());

        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: root.path(),
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: false,
                include_project: false,
                include_local: false,
            },
            RawSupplementalLayers {
                cli_layer: None,
                managed_policy: Some(&policy),
            },
            None,
        )
        .expect("policy read failure does not fail unrelated ordinary layer loading");

        assert!(stack
            .layers
            .iter()
            .all(|layer| layer.source != Source::Managed));
        let retained = stack
            .managed_policy_selection
            .as_ref()
            .expect("incomplete policy read remains observable");
        assert!(!retained.complete);
        assert_eq!(retained.unresolved.len(), 1);
        assert_eq!(
            retained.unresolved[0].reason,
            "source did not provide a projection (DidNotLoad)"
        );
    }

    #[test]
    fn raw_evidence_keeps_schema_rejected_tier_out_of_admitted_skill_overrides() {
        let root = tempfile::tempdir().unwrap();
        let user_home = root.path().join("user");
        let user_dir = user_home.join(".lingxi");
        let project_dir = root.path().join("project");
        let project_settings_dir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&project_settings_dir).unwrap();

        let user_path = user_dir.join("settings.json");
        std::fs::write(
            &user_path,
            r#"{"skillOverrides":{"user-only":"off","shared":"on","odd-\ud800":"name-only"}}"#,
        )
        .unwrap();
        let project_path = project_settings_dir.join("settings.json");
        std::fs::write(
            &project_path,
            r#"{"skillListingBudgetFraction":null,"skillOverrides":{"project-only":"off","shared":"user-invocable-only"}}"#,
        )
        .unwrap();

        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: &project_dir,
                defaults: SettingsJson::default(),
            },
            FileLayerScope::ALL,
            RawSupplementalLayers::default(),
            Some(&user_path),
        )
        .expect("valid raw settings JSON is retained even when a known field rejects it");

        assert!(stack
            .layers
            .iter()
            .any(|layer| layer.source == Source::Project
                && layer.path.as_deref() == Some(project_path.as_path())));
        assert_eq!(
            stack.merged.string_at("/skillOverrides/project-only"),
            Some("off"),
            "raw merged evidence retains the schema-rejected layer"
        );

        let admitted = stack.admitted_typed_layers();
        assert!(admitted.iter().all(|layer| layer.source != Source::Project));
        let user = admitted
            .iter()
            .find(|layer| layer.source == Source::User)
            .expect("valid user tier is admitted");
        let overrides = user
            .settings
            .skill_overrides
            .as_ref()
            .expect("user overrides");
        let odd_key = "odd-".encode_utf16().chain([0xd800]).collect::<Vec<_>>();
        assert_eq!(
            overrides.get_units(&odd_key),
            Some(crate::settings::schema::SkillOverride::NameOnly)
        );
        assert_eq!(
            overrides.get_units(&"project-only".encode_utf16().collect::<Vec<_>>()),
            None,
            "schema-rejected project values do not enter the typed admitted view"
        );
    }

    #[test]
    fn admitted_raw_merge_excludes_rejected_tier_and_keeps_source_units() {
        let root = tempfile::tempdir().unwrap();
        let user_home = root.path().join("user");
        let user_dir = user_home.join(".lingxi");
        let project_dir = root.path().join("project");
        let project_settings_dir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&project_settings_dir).unwrap();

        let user_path = user_dir.join("settings.json");
        std::fs::write(
            &user_path,
            r#"{"sandbox":{"filesystem":{"allowWrite":["/user-\ud800"]}},"futureSetting":{"label":"kept-passthrough"}}"#,
        )
        .unwrap();
        let project_path = project_settings_dir.join("settings.json");
        std::fs::write(
            &project_path,
            r#"{"sandbox":{"filesystem":{"allowWrite":["/rejected-project"]}},"skillListingBudgetFraction":null}"#,
        )
        .unwrap();

        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: &project_dir,
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: true,
                include_project: true,
                include_local: false,
            },
            RawSupplementalLayers::default(),
            Some(&user_path),
        )
        .expect("raw source layers load");

        assert!(stack
            .layers
            .iter()
            .any(|layer| layer.source == Source::Project));
        assert!(stack
            .admitted_raw_layers
            .iter()
            .all(|layer| layer.source != Source::Project));
        assert_eq!(
            stack
                .admitted_raw_layers
                .iter()
                .map(|layer| layer.source)
                .collect::<Vec<_>>(),
            vec![Source::Defaults, Source::User, Source::Env],
            "accepted raw tiers retain the same source precedence"
        );
        let allow_write = "/sandbox/filesystem/allowWrite";
        let user_units = "/user-".encode_utf16().chain([0xd800]).collect::<Vec<_>>();
        assert_eq!(
            stack.merged.array_len(allow_write),
            Some(2),
            "all-source evidence includes both syntactically valid source arrays"
        );
        assert_eq!(
            stack
                .merged
                .string_units("/sandbox/filesystem/allowWrite/0"),
            Some(user_units.clone()),
            "the lower user entry remains first in Native's array merge"
        );
        assert_eq!(
            stack.merged.string_at("/sandbox/filesystem/allowWrite/1"),
            Some("/rejected-project"),
            "all-source evidence retains the schema-rejected project's raw value"
        );
        assert_eq!(
            stack.admitted_merged().array_len(allow_write),
            Some(1),
            "the prompt-facing raw merge excludes the rejected project's array entry"
        );
        assert_eq!(
            stack
                .admitted_merged()
                .string_units("/sandbox/filesystem/allowWrite/0"),
            Some(user_units),
            "the admitted source keeps the user's exact UTF-16 units"
        );
        assert!(
            stack
                .admitted_merged()
                .at_pointer("/sandbox/filesystem/allowWrite/1")
                .is_none(),
            "the rejected project value is absent from the admitted merge"
        );
        assert_eq!(
            stack.admitted_merged().string_units("/futureSetting/label"),
            Some("kept-passthrough".encode_utf16().collect::<Vec<_>>()),
            "accepted passthrough properties remain source-preserving"
        );
    }

    #[test]
    fn null_disable_bundled_skills_rejects_the_whole_tier_but_preserves_raw_evidence() {
        let root = tempfile::tempdir().unwrap();
        let env = BTreeMap::new();
        let stack = Settings::load_raw_layer_stack_from_user_path(
            LoadInputs {
                env: &env,
                project_dir: root.path(),
                defaults: SettingsJson::default(),
            },
            FileLayerScope {
                include_user: false,
                include_project: false,
                include_local: false,
            },
            RawSupplementalLayers {
                cli_layer: Some(
                    r#"{"disableBundledSkills":null,"skillOverrides":{"kept-only-as-raw":"off"}}"#,
                ),
                managed_policy: None,
            },
            None,
        )
        .expect("syntactically valid CLI source is retained as evidence");

        let cli = stack
            .layers
            .iter()
            .find(|layer| layer.source == Source::Cli)
            .expect("CLI source is retained as raw evidence");
        assert!(cli
            .projection
            .at_pointer("/disableBundledSkills")
            .expect("explicit null remains in the original source tier")
            .is_null());
        assert!(
            stack.merged.at_pointer("/disableBundledSkills").is_none(),
            "raw merge treats null as absent; inspect the source layer for exact evidence"
        );
        assert_eq!(
            stack.merged.string_at("/skillOverrides/kept-only-as-raw"),
            Some("off")
        );
        assert!(stack
            .admitted_typed_layers()
            .iter()
            .all(|layer| layer.source != Source::Cli));
    }
}
