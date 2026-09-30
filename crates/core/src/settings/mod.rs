//! Settings loader and merge orchestration.
//!
//! Entry point: [`Settings::load`]. Per-field merge rules live in
//! [`merger`]; provenance for `/doctor` (M6) lives in [`tracer`].

use std::path::PathBuf;

pub mod company_announcements;
pub mod enterprise;
pub mod env_parser;
pub mod loader;
pub mod merger;
pub mod schema;
pub mod tracer;

/// Test-only support — process-wide mutex shared by every settings test that
/// mutates `HOME` to redirect [`loader::user_settings_path`] at a tempdir.
/// All such tests must lock this before calling `std::env::set_var("HOME", ...)`
/// so the parallel test runner can't observe another test's `HOME`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    pub(crate) static HOME_LOCK: Mutex<()> = Mutex::new(());
}

/// Errors returned by [`Settings::load`] and its sub-modules.
///
/// Mirrors spec §5 `SettingsError`.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// A settings file at this path could not be found.
    #[error("settings file not found: {0}")]
    Missing(PathBuf),
    /// JSON parse failure for the file at this path.
    #[error("settings file malformed at {path}: {source}")]
    ParseError {
        /// The file whose JSON failed to parse.
        path: PathBuf,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// An env var holds a value that can't be coerced into its target type.
    #[error("env var {var} has invalid value {value:?}")]
    InvalidEnv {
        /// The env var name (e.g. `LINGXI_TRUSTED_DIRECTORIES`).
        var: String,
        /// The raw value as received from the process env.
        value: String,
    },
    /// Semantic validation ([`SettingsJson::validate`]) rejected the file
    /// (e.g. empty string in an array field). Unknown fields are NOT a
    /// violation — they are tolerated-and-ignored (zod `.passthrough()`
    /// parity, `types.ts:1072`).
    #[error("schema validation failed: {0}")]
    SchemaViolation(String),
    /// Underlying IO failure (permission denied, etc.).
    #[error("io error reading {path}: {source}")]
    Io {
        /// The file the I/O happened on.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

pub use schema::{ProviderRegion, SettingsJson};

/// Observes settings-load outcomes without coupling the shared core to a
/// particular analytics transport or its privacy-tagged wire format.
#[async_trait::async_trait]
pub trait SettingsLoadObserver: Send + Sync {
    /// A setting supplied through the environment could not be parsed.
    async fn invalid_env(&self, var: String, value: String);
    /// A settings file was skipped after a read or validation error.
    async fn parse_error(&self, path: &std::path::Path, class: &'static str);
    /// The merge completed, including the count of contributing layers.
    async fn loaded(
        &self,
        layers_present: i64,
        had_env_override: bool,
        user_path: Option<&std::path::Path>,
    );
}

/// Inputs to [`Settings::load`].
///
/// We pass env in explicitly rather than calling `std::env::vars()` so unit
/// tests can construct a deterministic snapshot.
#[derive(Debug)]
pub struct LoadInputs<'a> {
    /// Process env snapshot.
    pub env: &'a std::collections::BTreeMap<String, String>,
    /// Project root — used to locate `<project_dir>/.lingxi/settings.json`.
    pub project_dir: &'a std::path::Path,
    /// Defaults baseline. Lowest priority.
    pub defaults: SettingsJson,
}

/// Which on-disk settings files are allowed to contribute to the merged view.
///
/// This matches Claude Code's three file-backed setting sources:
/// - user: `~/.lingxi/settings.json`
/// - project: `<project>/.lingxi/settings.json`
/// - local: `<project>/.lingxi/settings.local.json`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileLayerScope {
    /// Whether `~/.lingxi/settings.json` contributes.
    pub include_user: bool,
    /// Whether `<project>/.lingxi/settings.json` contributes.
    pub include_project: bool,
    /// Whether `<project>/.lingxi/settings.local.json` contributes.
    pub include_local: bool,
}

impl FileLayerScope {
    /// Enable all three file-backed settings sources.
    pub const ALL: Self = Self {
        include_user: true,
        include_project: true,
        include_local: true,
    };
}

impl Default for FileLayerScope {
    fn default() -> Self {
        Self::ALL
    }
}

/// Optional non-file settings layers that sit above local/project/user.
///
/// `cli_layer` is the parsed `--settings` / `flagSettings` payload.
/// `managed_layers` are the managed `policySettings` tiers in ASCENDING
/// priority, so later entries override earlier ones.
#[derive(Debug, Clone, Copy, Default)]
pub struct SupplementalLayers<'a> {
    /// Parsed CLI `--settings` / `flagSettings` layer.
    pub cli_layer: Option<&'a SettingsJson>,
    /// Managed `policySettings` tiers in ascending priority order.
    pub managed_layers: &'a [SettingsJson],
}

/// Result of [`Settings::load`].
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveSettings {
    /// The merged settings.
    pub settings: SettingsJson,
    /// Per-field provenance trace for `/doctor` (M6).
    pub trace: tracer::ProvenanceTrace,
}

impl EffectiveSettings {
    /// Look up provenance for a top-level settings field.
    ///
    /// Field names are camelCase wire identifiers (e.g. `trustedDirectories`),
    /// matching the JSON keys — NOT the Rust `snake_case` field names.
    /// Returns `None` if no layer ever set this field.
    #[must_use]
    pub fn effective_for(&self, field: &str) -> Option<&tracer::FieldProvenance> {
        self.trace.by_field.get(field)
    }
}

/// Public entry point. See spec §4 Flow D for the data-flow diagram.
#[non_exhaustive]
pub struct Settings;

impl Settings {
    /// Load the merged defaults + user + project + local + env settings.
    ///
    /// Priority (highest first): env → local → project → user → defaults.
    /// Merge order (call order in code): defaults → user → project → local
    /// → env, because [`merger::merge`] is `(prev, next)` where `next`
    /// overrides.
    ///
    /// # Errors
    ///
    /// Any [`SettingsError`] from a sub-layer bubbles up. Missing settings
    /// files are NOT errors — they just contribute an empty layer.
    pub fn load(inputs: LoadInputs<'_>) -> Result<EffectiveSettings, SettingsError> {
        Self::load_with_layers(inputs, FileLayerScope::ALL, SupplementalLayers::default())
    }

    /// Load settings with explicit file-source gating plus optional CLI and
    /// managed layers.
    ///
    /// Answers: which settings VALUE wins a conflict.
    /// One of several orderings over these rungs; `crate::types::scope`'s module docs index them all and say which question each answers.
    ///
    /// Priority (highest first): env → managed → cli → local → project
    /// → user → defaults. `managed_layers` must already be sorted in ASCENDING
    /// priority so later tiers override earlier ones.
    ///
    /// # Errors
    ///
    /// Same as [`Settings::load`].
    pub fn load_with_layers(
        inputs: LoadInputs<'_>,
        file_scope: FileLayerScope,
        supplemental: SupplementalLayers<'_>,
    ) -> Result<EffectiveSettings, SettingsError> {
        let user_path = loader::user_settings_path();
        Self::load_with_layers_from_user_path(
            inputs,
            file_scope,
            supplemental,
            user_path.as_deref(),
        )
    }

    /// Load the canonical layer stack while explicitly selecting the user
    /// settings file. Hosts with a custom config directory use this instead of
    /// relying on process-global `HOME`/config-directory environment state.
    ///
    /// Passing `None` omits the user layer even when `file_scope.include_user`
    /// is true. All other precedence and provenance semantics are identical to
    /// [`Settings::load_with_layers`].
    ///
    /// # Errors
    ///
    /// Same as [`Settings::load`].
    pub fn load_with_layers_from_user_path(
        inputs: LoadInputs<'_>,
        file_scope: FileLayerScope,
        supplemental: SupplementalLayers<'_>,
        user_settings_path: Option<&std::path::Path>,
    ) -> Result<EffectiveSettings, SettingsError> {
        let LoadInputs {
            env,
            project_dir,
            defaults,
        } = inputs;

        let mut trace = tracer::ProvenanceTrace::default();

        // Layer 1 (lowest): defaults
        trace.record_layer(tracer::Source::Defaults, &defaults);
        let mut acc = defaults;

        // Layer 2: user. (review #2) A single unreadable/oversized/invalid file
        // is SKIPPED (via `read_layer_or_skip`) rather than aborting the whole
        // merge — claude-code "skips files with errors entirely, not just the
        // invalid settings" and keeps merging the remaining sources, so one bad
        // user file never discards valid project/local/env layers.
        if file_scope.include_user {
            if let Some(user_path) = user_settings_path {
                if let Some(usr) = read_layer_or_skip(user_path) {
                    trace.record_layer(tracer::Source::User, &usr);
                    acc = merger::merge(acc, usr);
                }
            }
        }

        // Layer 3: project
        if file_scope.include_project {
            let project_path = loader::project_settings_path(project_dir);
            if let Some(proj) = read_layer_or_skip(&project_path) {
                trace.record_layer(tracer::Source::Project, &proj);
                acc = merger::merge(acc, proj);
            }
        }

        // Layer 4: project-local
        if file_scope.include_local {
            let local_path = loader::local_settings_path(project_dir);
            if let Some(local) = read_layer_or_skip(&local_path) {
                trace.record_layer(tracer::Source::Local, &local);
                acc = merger::merge(acc, local);
            }
        }

        // Layer 5: CLI / flagSettings
        if let Some(cli) = supplemental.cli_layer {
            if *cli != SettingsJson::default() {
                trace.record_layer(tracer::Source::Cli, cli);
                acc = merger::merge(acc, cli.clone());
            }
        }

        // Layer 6: managed / policySettings
        for managed in supplemental.managed_layers {
            if *managed != SettingsJson::default() {
                trace.record_layer(tracer::Source::Managed, managed);
                acc = merger::merge(acc, managed.clone());
            }
        }

        // Layer 7 (highest): env
        let (env_layer, _invalid_env) = env_parser::parse_env(env)?;
        trace.record_layer(tracer::Source::Env, &env_layer);
        acc = merger::merge(acc, env_layer);

        Ok(EffectiveSettings {
            settings: acc,
            trace,
        })
    }

    /// Like [`Settings::load`] but GATES the user / project file layers — the
    /// substrate for claude-code's `--setting-sources <user,project,local>`
    /// (scope which setting sources load). `include_user` selects whether the
    /// user settings layer contributes. `include_project` gates BOTH the shared
    /// project settings file and the project-local `settings.local.json` layer,
    /// preserving the existing two-flag API surface. Callers that need a
    /// distinct local toggle should use [`Settings::load_with_layers`].
    ///
    /// # Errors
    /// Same as [`Settings::load`].
    pub fn load_scoped(
        inputs: LoadInputs<'_>,
        include_user: bool,
        include_project: bool,
    ) -> Result<EffectiveSettings, SettingsError> {
        Self::load_with_layers(
            inputs,
            FileLayerScope {
                include_user,
                include_project,
                include_local: include_project,
            },
            SupplementalLayers::default(),
        )
    }

    /// Same as [`Settings::load`] but reports diagnostics to an observer.
    ///
    /// This method is async because the observer may perform asynchronous work.
    ///
    /// The observer receives one completion event, one event per invalid
    /// environment variable, and one event per settings file skipped after an
    /// error. The telemetry crate formats these as analytics events.
    ///
    /// `observer = None` disables diagnostic callbacks.
    ///
    /// # Errors
    ///
    /// Same as [`Settings::load`]. An unreadable or invalid file is skipped and
    /// reported through the observer before the remaining layers are merged.
    #[allow(clippy::too_many_lines)]
    pub async fn load_with_observer(
        inputs: LoadInputs<'_>,
        observer: Option<&dyn SettingsLoadObserver>,
    ) -> Result<EffectiveSettings, SettingsError> {
        Self::load_with_observer_layers(
            inputs,
            FileLayerScope::ALL,
            SupplementalLayers::default(),
            observer,
        )
        .await
    }

    /// Async diagnostic variant of [`Settings::load_with_layers`].
    #[allow(clippy::too_many_lines)]
    pub async fn load_with_observer_layers(
        inputs: LoadInputs<'_>,
        file_scope: FileLayerScope,
        supplemental: SupplementalLayers<'_>,
        observer: Option<&dyn SettingsLoadObserver>,
    ) -> Result<EffectiveSettings, SettingsError> {
        let LoadInputs {
            env,
            project_dir,
            defaults,
        } = inputs;
        let mut trace = tracer::ProvenanceTrace::default();
        let mut layers_present: i64 = 0;
        let user_path = loader::user_settings_path();
        let mut had_env_override = false;

        // Layer 1 (lowest): defaults
        trace.record_layer(tracer::Source::Defaults, &defaults);
        let mut acc = defaults;
        layers_present += 1;

        // Layer 2: user
        if file_scope.include_user {
            if let Some(ref up) = user_path {
                match loader::read_settings_file(up) {
                    Ok(Some(usr)) => {
                        trace.record_layer(tracer::Source::User, &usr);
                        acc = merger::merge(acc, usr);
                        layers_present += 1;
                    }
                    Ok(None) => {}
                    // (review #2) Skip a bad file, keep merging the rest (parity:
                    // claude-code skips files with errors entirely). The error is
                    // still surfaced via telemetry; it just no longer discards the
                    // other layers.
                    Err(e) => {
                        emit_parse_error(observer, up, &e).await;
                    }
                }
            }
        }

        // Layer 3: project
        if file_scope.include_project {
            let project_path = loader::project_settings_path(project_dir);
            match loader::read_settings_file(&project_path) {
                Ok(Some(proj)) => {
                    trace.record_layer(tracer::Source::Project, &proj);
                    acc = merger::merge(acc, proj);
                    layers_present += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    emit_parse_error(observer, &project_path, &e).await;
                }
            }
        }

        // Layer 4: project-local
        if file_scope.include_local {
            let local_path = loader::local_settings_path(project_dir);
            match loader::read_settings_file(&local_path) {
                Ok(Some(local)) => {
                    trace.record_layer(tracer::Source::Local, &local);
                    acc = merger::merge(acc, local);
                    layers_present += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    emit_parse_error(observer, &local_path, &e).await;
                }
            }
        }

        // Layer 5: CLI / flagSettings
        if let Some(cli) = supplemental.cli_layer {
            if *cli != SettingsJson::default() {
                trace.record_layer(tracer::Source::Cli, cli);
                acc = merger::merge(acc, cli.clone());
                layers_present += 1;
            }
        }

        // Layer 6: managed / policySettings
        for managed in supplemental.managed_layers {
            if *managed != SettingsJson::default() {
                trace.record_layer(tracer::Source::Managed, managed);
                acc = merger::merge(acc, managed.clone());
                layers_present += 1;
            }
        }

        // Layer 7 (highest): env
        let (env_layer, invalid_env) = env_parser::parse_env(env)?;
        let env_was_nonempty = env_layer != SettingsJson::default();
        if env_was_nonempty {
            had_env_override = true;
            layers_present += 1;
        }
        trace.record_layer(tracer::Source::Env, &env_layer);
        acc = merger::merge(acc, env_layer);

        // Emit per-invalid-env event before the loaded event.
        if let Some(observer) = observer {
            for (var, value) in invalid_env {
                observer.invalid_env(var, value).await;
            }
        }

        // Emit the success event.
        if let Some(observer) = observer {
            observer
                .loaded(layers_present, had_env_override, user_path.as_deref())
                .await;
        }

        Ok(EffectiveSettings {
            settings: acc,
            trace,
        })
    }
}

/// Read one settings-file layer for the non-telemetry loader, returning `None`
/// when the file is absent OR unreadable/oversized/invalid. (review #2 / parity)
/// claude-code "skips files with errors entirely, not just the invalid settings"
/// and keeps merging the remaining sources, so a single bad file must never
/// discard valid project/local/env layers. The error is logged (never the raw
/// content) and swallowed; genuinely fatal conditions (env parse) are handled
/// separately by the callers and still abort.
fn read_layer_or_skip(path: &std::path::Path) -> Option<SettingsJson> {
    match loader::read_settings_file(path) {
        Ok(opt) => opt,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read raw settings from file; skipping this layer and continuing the merge"
            );
            None
        }
    }
}

/// Report only the error class for a skipped file. The observer owns the
/// privacy treatment of its path; raw error text may contain file content.
async fn emit_parse_error(
    observer: Option<&dyn SettingsLoadObserver>,
    path: &std::path::Path,
    err: &SettingsError,
) {
    let Some(observer) = observer else { return };
    // Structural error class only — never the raw error message which may
    // echo file content.
    let class = match err {
        SettingsError::ParseError { .. } => "parse",
        SettingsError::SchemaViolation(_) => "schema",
        SettingsError::Io { .. } => "io",
        SettingsError::Missing(_) => "missing",
        SettingsError::InvalidEnv { .. } => "invalid_env",
    };
    observer.parse_error(path, class).await;
}

#[cfg(test)]
mod load_tests {
    use super::*;
    use crate::settings::test_support::HOME_LOCK;
    use std::collections::BTreeMap;
    use std::io::Write;

    #[test]
    fn env_beats_local_beats_project_beats_user_beats_defaults() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        let project_subdir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();
        let local_path = project_subdir.join("settings.local.json");

        let mut pf = std::fs::File::create(project_subdir.join("settings.json")).unwrap();
        writeln!(pf, r#"{{"model": "project-model"}}"#).unwrap();

        let mut uf = std::fs::File::create(user_dir.join("settings.json")).unwrap();
        writeln!(uf, r#"{{"model": "user-model"}}"#).unwrap();
        let mut lf = std::fs::File::create(local_path).unwrap();
        writeln!(lf, r#"{{"model": "local-model"}}"#).unwrap();

        // Stand in for $HOME so user_settings_path() points to our tempdir.
        std::env::set_var("HOME", tmp.path().join("home"));

        let mut env = BTreeMap::new();
        env.insert("LINGXI_MODEL".to_string(), "env-model".to_string());

        let defaults = schema::SettingsJson {
            model: Some("default-model".to_string()),
            ..Default::default()
        };

        let eff = Settings::load(LoadInputs {
            env: &env,
            project_dir,
            defaults,
        })
        .unwrap();
        assert_eq!(eff.settings.model.as_deref(), Some("env-model"));
    }

    #[test]
    fn project_beats_user_when_no_local_or_env() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home2").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        let project_subdir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();

        let mut pf = std::fs::File::create(project_subdir.join("settings.json")).unwrap();
        writeln!(pf, r#"{{"model": "project-model"}}"#).unwrap();
        let mut uf = std::fs::File::create(user_dir.join("settings.json")).unwrap();
        writeln!(uf, r#"{{"model": "user-model"}}"#).unwrap();

        std::env::set_var("HOME", tmp.path().join("home2"));

        let eff = Settings::load(LoadInputs {
            env: &BTreeMap::new(),
            project_dir,
            defaults: schema::SettingsJson::default(),
        })
        .unwrap();
        assert_eq!(eff.settings.model.as_deref(), Some("project-model"));
    }

    #[test]
    fn array_concat_dedup_runs_through_defaults_user_project_local_env_layers() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home3").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        let project_subdir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();
        let local_path = project_subdir.join("settings.local.json");

        let mut pf = std::fs::File::create(project_subdir.join("settings.json")).unwrap();
        writeln!(pf, r#"{{"trustedDirectories": ["/project"]}}"#).unwrap();
        let mut uf = std::fs::File::create(user_dir.join("settings.json")).unwrap();
        writeln!(uf, r#"{{"trustedDirectories": ["/user"]}}"#).unwrap();
        let mut lf = std::fs::File::create(local_path).unwrap();
        writeln!(lf, r#"{{"trustedDirectories": ["/local", "/project"]}}"#).unwrap();
        std::env::set_var("HOME", tmp.path().join("home3"));

        let mut env = BTreeMap::new();
        env.insert("LINGXI_TRUSTED_DIRECTORIES".to_string(), "/env".to_string());

        let defaults = schema::SettingsJson {
            trusted_directories: Some(vec!["/default".into()]),
            ..Default::default()
        };

        let eff = Settings::load(LoadInputs {
            env: &env,
            project_dir,
            defaults,
        })
        .unwrap();
        assert_eq!(
            eff.settings.trusted_directories.as_deref(),
            Some(
                &[
                    "/default".to_string(),
                    "/user".to_string(),
                    "/project".to_string(),
                    "/local".to_string(),
                    "/env".to_string()
                ][..]
            ),
            "all file layers plus env contribute in low-to-high priority order"
        );
    }

    #[test]
    fn load_with_layers_honors_local_only_scope() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home_scope").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        let project_subdir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();

        std::fs::write(user_dir.join("settings.json"), r#"{"model": "user-model"}"#).unwrap();
        std::fs::write(
            project_subdir.join("settings.json"),
            r#"{"model": "project-model"}"#,
        )
        .unwrap();
        std::fs::write(
            project_subdir.join("settings.local.json"),
            r#"{"model": "local-model"}"#,
        )
        .unwrap();
        std::env::set_var("HOME", tmp.path().join("home_scope"));

        let eff = Settings::load_with_layers(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir,
                defaults: schema::SettingsJson::default(),
            },
            FileLayerScope {
                include_user: false,
                include_project: false,
                include_local: true,
            },
            SupplementalLayers::default(),
        )
        .unwrap();
        assert_eq!(eff.settings.model.as_deref(), Some("local-model"));
    }

    #[test]
    fn explicit_user_path_supports_custom_config_home() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("project");
        let custom_home = tmp.path().join("custom-config");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::create_dir_all(&custom_home).unwrap();
        let user_path = custom_home.join("settings.json");
        std::fs::write(&user_path, r#"{"workflowSizeGuideline":"large"}"#).unwrap();

        let eff = Settings::load_with_layers_from_user_path(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: &project_dir,
                defaults: SettingsJson {
                    workflow_size_guideline: Some("medium".to_string()),
                    ..Default::default()
                },
            },
            FileLayerScope {
                include_user: true,
                include_project: false,
                include_local: false,
            },
            SupplementalLayers::default(),
            Some(&user_path),
        )
        .unwrap();

        assert_eq!(
            eff.settings.workflow_size_guideline.as_deref(),
            Some("large")
        );
        assert_eq!(
            eff.effective_for("workflowSizeGuideline")
                .and_then(|source| source.contributors.last()),
            Some(&tracer::Source::User)
        );
    }

    #[test]
    fn load_with_layers_gives_managed_precedence_without_dropping_project_provider_extensions() {
        use serde_json::json;

        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        let user_dir = tmp.path().join("home_layers").join(".lingxi");
        std::fs::create_dir_all(&user_dir).unwrap();
        let project_subdir = project_dir.join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();

        std::fs::write(
            user_dir.join("settings.json"),
            r#"{"model":"user-model","workflowSizeGuideline":"small","providers":{"userOnly":{"type":"openai"}}}"#,
        )
        .unwrap();
        std::fs::write(
            project_subdir.join("settings.json"),
            r#"{"model":"project-model","workflowSizeGuideline":"large","providers":{"projectOnly":{"baseUrl":"https://project.example"},"shared":{"baseUrl":"https://project.example"}}}"#,
        )
        .unwrap();
        std::fs::write(
            project_subdir.join("settings.local.json"),
            r#"{"model":"local-model","workflowSizeGuideline":"small","providers":{"localOnly":{"apiKeyEnv":"LOCAL_KEY"},"shared":{"apiKeyEnv":"LOCAL_KEY"}}}"#,
        )
        .unwrap();
        std::env::set_var("HOME", tmp.path().join("home_layers"));

        let cli_layer: SettingsJson = serde_json::from_value(json!({
            "model": "cli-model",
            "workflowSizeGuideline": "large",
            "providers": {
                "cliOnly": { "type": "openai" },
                "shared": { "timeout": 30 }
            }
        }))
        .unwrap();
        let managed_layer: SettingsJson = serde_json::from_value(json!({
            "model": "managed-model",
            "workflowSizeGuideline": "medium",
            "providers": {
                "managedOnly": { "region": "managed" },
                "shared": { "region": "managed" }
            }
        }))
        .unwrap();

        let eff = Settings::load_with_layers(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir,
                defaults: schema::SettingsJson::default(),
            },
            FileLayerScope::ALL,
            SupplementalLayers {
                cli_layer: Some(&cli_layer),
                managed_layers: std::slice::from_ref(&managed_layer),
            },
        )
        .unwrap();
        assert_eq!(eff.settings.model.as_deref(), Some("managed-model"));
        assert_eq!(
            eff.settings.workflow_size_guideline.as_deref(),
            Some("medium"),
            "managed > flag > local > project > user"
        );
        assert_eq!(
            eff.effective_for("workflowSizeGuideline")
                .and_then(|source| source.contributors.last()),
            Some(&tracer::Source::Managed)
        );

        let providers = eff.settings.providers.unwrap_or_default();
        assert!(providers.contains_key("userOnly"));
        assert!(providers.contains_key("projectOnly"));
        assert!(providers.contains_key("localOnly"));
        assert!(providers.contains_key("cliOnly"));
        assert!(providers.contains_key("managedOnly"));
        assert_eq!(
            providers.get("shared"),
            Some(&json!({
                "baseUrl": "https://project.example",
                "apiKeyEnv": "LOCAL_KEY",
                "timeout": 30,
                "region": "managed"
            }))
        );
    }
}
