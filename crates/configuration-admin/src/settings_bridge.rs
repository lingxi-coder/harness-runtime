//! Settings snapshot builder.
//!
//! Lives in `bridge-server` rather than `lingxi_core::settings` because the
//! `core` crate's dependencies are deliberately minimal (no `traits`, no
//! `permission`), while `bridge-server` already depends on client-protocol,
//! permission, migrations, traits and engine — `permission::mark_internal_write`
//! is needed by [`apply_patch`] before the shared migrations writer publishes.
//!
//! This module reads and merges the layered settings files into one snapshot
//! (the effective/merged values, plus which layer each value actually came
//! from) and writes shallow top-level patches back to one writable layer via
//! [`apply_patch`], marked so the desktop's own save is not mistaken for an
//! external edit.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use client_protocol::commands::WritableScopeDto;
use lingxi_core::settings::env_parser;
use lingxi_core::settings::merger::merge_raw_layer;
use migrations::settings_update::{read_settings_map, settings_path, WritableScope};
use serde_json::Value;

/// A layer a settings value can come from.
///
/// The merge precedence these layers fold in (`env → managed → cli → local →
/// project → user → defaults`, highest first) is deliberately NOT a property of
/// this type — it is one of several conflicting orders over the same rungs, and
/// it lives in `core::settings`'s fold. The file-layer subset this module reads
/// is ordered by [`FILE_LAYERS`] below.
pub use protocol::Scope as SettingsLayer;

/// One settings file's on-disk state, as surfaced to the UI.
pub struct SettingsFile {
    /// Which layer this file backs.
    pub layer: SettingsLayer,
    /// The resolved on-disk path, regardless of whether it exists.
    pub path: PathBuf,
    /// Whether the file exists on disk.
    pub exists: bool,
    /// Whether the file parsed cleanly and can be written to (`false` when
    /// `parse_error` is set).
    pub writable: bool,
    /// The parse error, if the file exists but contains invalid JSON.
    pub parse_error: Option<String>,
}

/// A merged view of the layered settings: the effective values, which layer
/// each one actually came from, the session's active (already-loaded)
/// values, and which keys an administrator has locked via the managed layer.
pub struct SettingsSnapshot {
    /// The on-disk state of every file layer this module reads.
    pub files: Vec<SettingsFile>,
    /// The merged values, resolved the way the running engine resolves them:
    /// every layer folded through [`merge_raw_layer`], which reads the
    /// engine's own per-field strategy table. For most keys that IS "the
    /// value from its highest-priority contributing layer", but for a key the
    /// engine deep-merges (`hooks`, `permissions`, `providers`, …) or
    /// concat-dedups (`trustedDirectories`, …) it is a UNION across layers —
    /// see [`SettingsSnapshot::merged_keys`], which names exactly those keys.
    pub effective: BTreeMap<String, Value>,
    /// The values this session actually had loaded at session start: the
    /// three file layers PLUS the managed (policy) overlay — but NOT `cli`
    /// or `env`. Supplied by the caller, not computed here (see
    /// `active_settings_baseline` for the intended way to build it).
    ///
    /// Managed is included on purpose: the running engine loads managed
    /// settings at startup exactly as much as it loads the three files, so a
    /// value here that left managed out would make a policy-pinned key
    /// differ from `effective` (which always re-applies the SAME overlay on
    /// every later listing) forever — reporting it as eternally "pending a
    /// restart" when no restart could ever apply it, since the user never
    /// wrote it and no restart changes it. This field answers "what did this
    /// session load at boot" — the baseline a pending-changes diff is taken
    /// against — and managed settings are part of that answer.
    pub active: BTreeMap<String, Value>,
    /// For each key in `effective`, the highest-priority layer that defines
    /// it. For a key in [`SettingsSnapshot::merged_keys`] this is NOT where
    /// the effective value came from — the value came from several layers at
    /// once — so a UI must consult `merged_keys` before it renders a
    /// single-layer badge from this map.
    pub provenance: BTreeMap<String, SettingsLayer>,
    /// The keys whose effective value is a CROSS-LAYER union rather than one
    /// layer's value: the engine deep-merges or concat-dedups them (its
    /// `schema::MERGE_STRATEGIES` table), and more than one layer contributed.
    ///
    /// Naming a single layer for these in `provenance` would be false, which
    /// is why they are called out separately rather than folded into it: a
    /// value assembled from `user` + `project` did not come from either one.
    /// A key is listed only when the merge actually produced something no
    /// single layer holds — a deep-merge key whose entries the winning layer
    /// entirely redefines is NOT listed, because for that key the winning
    /// layer's badge is honest (see [`merge_raw_layer`]'s own doc).
    pub merged_keys: Vec<String>,
    /// Keys an administrator pinned through the managed-settings layer —
    /// exactly the keys of the `managed` overlay `build_snapshot` was given,
    /// so every one of them also appears in `effective` with a `Managed`
    /// provenance. (The one exception is an overlay entry whose value is JSON
    /// `null`, which the engine reads as unset and which therefore reaches
    /// neither `effective` nor `provenance` — see [`merge_raw_layer`].)
    ///
    /// "Pinned" is about who controls the key, not about the whole value
    /// being the policy's: the engine folds the managed layer in through the
    /// SAME merger as every other layer, so a pinned key the engine
    /// deep-merges (`permissions`, say) resolves to the policy's entries
    /// UNIONED with the file layers' — such a key appears here AND in
    /// [`SettingsSnapshot::merged_keys`].
    pub locked: Vec<String>,
    /// Each FILE layer's OWN raw map, unmerged: `{"user": {...}, "project":
    /// {...}, "local": {...}}`, keyed by [`layer_wire_name`]. This
    /// is what a layered editor must pre-merge an object-valued key's write
    /// against — `effective` is a cross-layer merge and can carry another
    /// layer's entries for a key like `providers`, so basing a write on it
    /// would silently fork that other layer's data into whichever layer gets
    /// saved (see `ClientEvent::SettingsSnapshot::layers_json`'s doc). A
    /// layer whose file does not exist or failed to parse contributes an
    /// empty map here, the same way it contributes nothing to `effective`.
    pub layers: BTreeMap<&'static str, serde_json::Map<String, Value>>,
}

/// The two roots needed to resolve every settings layer's file path.
pub struct SettingsPaths {
    /// The user's LingXi home directory (holds the `User` layer file).
    pub lingxi_home: PathBuf,
    /// The current project's root directory (holds the `Project` and
    /// `Local` layer files, under `<project_dir>/<DOT_DIR>/`).
    pub project_dir: PathBuf,
}

/// The FILE layers this module reads and merges, ordered lowest-priority
/// first so a later entry overwrites an earlier one. Per the engine's merge
/// precedence, among these three: local beats project beats user. (`cli`,
/// `managed`, `env` and `defaults` are not file layers this module reads.)
/// Answers: which settings files the admin panel reads, and in what order.
///
/// One of several orderings over these rungs; `protocol::scope`'s module docs index them all and say which question each answers.
const FILE_LAYERS: [WritableScope; 3] = [
    WritableScope::User,
    WritableScope::Project,
    WritableScope::Local,
];

/// Build a settings snapshot by reading and merging the three writable file
/// layers, then applying the administrator's managed (policy) overlay on top.
///
/// `active` is the file-layer values as read at session start (the caller
/// supplies it; this function does not compute it). `managed` is the
/// administrator's key → value overlay, supplied by the composition root
/// (`harness_runtime::desktop::managed_settings_overlay`) — this module deliberately
/// does not discover managed layers itself.
///
/// The overlay is applied AFTER the file layers, never before, because managed
/// outranks all three in the engine's precedence
/// (`env → managed → cli → local → project → user → defaults`). Applying it
/// first would let a user/project/local file overwrite a policy value, so
/// `effective` would report a value that cannot actually win and `provenance`
/// would name the wrong layer — for exactly the keys reported as locked.
pub fn build_snapshot(
    paths: &SettingsPaths,
    active: BTreeMap<String, Value>,
    managed: BTreeMap<String, Value>,
) -> SettingsSnapshot {
    build_snapshot_with_env(paths, active, managed, &std::env::vars().collect())
}

fn build_snapshot_with_env(
    paths: &SettingsPaths,
    active: BTreeMap<String, Value>,
    managed: BTreeMap<String, Value>,
    env: &BTreeMap<String, String>,
) -> SettingsSnapshot {
    let mut effective: BTreeMap<String, Value> = BTreeMap::new();
    let mut provenance: BTreeMap<String, SettingsLayer> = BTreeMap::new();
    let mut merged_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut files: Vec<SettingsFile> = Vec::new();
    let mut layers: BTreeMap<&'static str, serde_json::Map<String, Value>> = BTreeMap::new();

    for source in FILE_LAYERS {
        let layer = SettingsLayer::from(source);
        let path = settings_path(source, &paths.lingxi_home, &paths.project_dir);
        let exists = path.exists();
        let (map, parse_error) = match read_settings_map(&path) {
            Ok(m) => (m, None),
            Err(e) => (serde_json::Map::new(), Some(e)),
        };
        for (key, value) in &map {
            if value.is_null() {
                // `"key": null` is UNSET to the engine — every `SettingsJson`
                // field is an `Option`, so it parses to `None` and never
                // reaches `effective` (see `merge_raw_layer`'s doc). Claiming
                // this layer as the provenance of a key it does not set would
                // put a name in the map with no value behind it.
                continue;
            }
            provenance.insert(key.clone(), layer);
            // Reset before this layer's own report goes in. A key an earlier
            // pair of layers unioned can be fully REDEFINED here — user
            // `{A}` + project `{B}` unions to `{A,B}`, and a local layer
            // defining both `A` and `B` shadows it back to exactly local's
            // own value. Accumulating across layers without this would keep
            // calling that merged and suppress a badge that is now honest;
            // the layers that follow always have the last word.
            merged_keys.remove(key);
        }
        merged_keys.extend(fold_layer(&mut effective, &map));
        files.push(SettingsFile {
            layer,
            path,
            exists,
            writable: parse_error.is_none(),
            parse_error,
        });
        // Moved, not cloned: `map` is only read by reference above, so this
        // is the map's one and only owner from here on.
        layers.insert(layer_wire_name(layer), map);
    }

    // The managed overlay goes on LAST so it wins, and its keys are the locked
    // set: locked is derived from the same map that supplied the values, so the
    // two can never disagree about which keys an administrator pinned.
    //
    // It goes through the SAME merge as the file layers, because the engine
    // does: `Settings::load` folds managed in with `merger::merge(acc,
    // managed)`, not with an overwrite. Applying it flat here would make a
    // policy-pinned key the engine deep-merges (`permissions`, `hooks`)
    // report a value the engine never resolves — the very defect this
    // function's file-layer loop was fixed for, kept alive one layer higher.
    let locked: Vec<String> = managed.keys().cloned().collect();
    for (key, value) in &managed {
        // Same null rule and same reset as the file-layer loop above.
        if value.is_null() {
            continue;
        }
        provenance.insert(key.clone(), SettingsLayer::Managed);
        merged_keys.remove(key);
    }
    merged_keys.extend(merge_raw_layer(&mut effective, managed));

    if active.contains_key("providerRegion") {
        effective
            .entry("providerRegion".into())
            .or_insert_with(|| Value::String("international".into()));
    }

    // Region controls affect routing at startup. Report the same validated
    // environment override so a saved value cannot look perpetually pending.
    if let Some(region) = env_parser::parse_env(env)
        .ok()
        .and_then(|(settings, _)| settings.provider_region)
    {
        effective.insert(
            "providerRegion".into(),
            serde_json::to_value(region).expect("region serializes"),
        );
        provenance.insert("providerRegion".into(), SettingsLayer::Env);
    }

    SettingsSnapshot {
        files,
        effective,
        active,
        provenance,
        merged_keys: merged_keys.into_iter().collect(),
        locked,
        layers,
    }
}

/// Fold one file layer's raw map into `effective` through the engine's merge,
/// returning the keys that became a cross-layer union.
///
/// A thin adapter, not a second merge: `read_settings_map` hands back a
/// `serde_json::Map` while [`merge_raw_layer`] takes the `BTreeMap` shape the
/// snapshot carries, and the caller still owns `map` afterwards (it becomes
/// that layer's entry in [`SettingsSnapshot::layers`]). Every merge decision
/// is made inside `merge_raw_layer`, in the engine.
fn fold_layer(
    effective: &mut BTreeMap<String, Value>,
    map: &serde_json::Map<String, Value>,
) -> Vec<String> {
    let owned: BTreeMap<String, Value> = map
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    merge_raw_layer(effective, owned)
}

/// The `SettingsContext.active` a connection captures ONCE at boot: the three
/// settings files merged, with the SAME managed overlay `effective` will keep
/// re-applying on every later listing folded in here too.
///
/// This is not "the file layers as read" — it is "what this session actually
/// had loaded at boot", and the engine loads managed (policy) settings at
/// boot exactly as much as it loads the three files. Baking `managed` in here
/// is what keeps a managed key from permanently differing between `active`
/// and every later `effective`: without it, a policy-pinned key would show up
/// forever in a diff of the two — a "restart to apply" banner for a change no
/// restart can ever resolve, since the user never wrote it and no restart
/// changes it. Taking `managed` by reference (rather than consuming it, the
/// way [`build_snapshot`]'s own parameter does) lets the caller reuse the
/// exact same map for the `SettingsContext.managed` field every later listing
/// re-applies — the same map in both places, so the two can never drift apart.
pub fn active_settings_baseline(
    paths: &SettingsPaths,
    managed: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    build_snapshot(paths, BTreeMap::new(), managed.clone()).effective
}

/// Resolve the file path for a **writable** destination layer. Only
/// `User` / `Project` / `Local` are writable; everything else is rejected
/// with an error naming both the rejected value and the writable set.
pub fn writable_path(paths: &SettingsPaths, layer: SettingsLayer) -> Result<PathBuf, String> {
    let source = match layer {
        SettingsLayer::User => WritableScope::User,
        SettingsLayer::Project => WritableScope::Project,
        SettingsLayer::Local => WritableScope::Local,
        other => {
            return Err(format!(
                "{other:?} is not a writable settings destination; writable destinations are User, Project, Local"
            ))
        }
    };
    Ok(settings_path(
        source,
        &paths.lingxi_home,
        &paths.project_dir,
    ))
}

/// Top-level keys with a dedicated writer, and therefore refused on the
/// generic patch path (I1). Permissions have their own writer
/// (`permission::persist`, routed by [`ClientCommand::UpdatePermissionRules`]
/// et al. — see the `permission_*` functions below); allowing the generic
/// path to also touch `permissions` would give the key two write paths, which
/// is exactly the defect this list exists to prevent.
///
/// The hint names all three replacement commands rather than one: the patch
/// is a SHALLOW, top-level replace of the whole `permissions` object, so at
/// this point there is no way to tell whether the caller actually meant
/// `permissions.allow/deny/ask` (→ `UpdatePermissionRules`),
/// `permissions.defaultMode` (→ `SetDefaultPermissionMode`), or
/// `permissions.additionalDirectories` (→ `UpdateWorkspaceDirectories`).
/// Naming only one would point a caller of the other two at the wrong
/// command.
const RESERVED_KEYS: [(&str, &str); 1] = [(
    "permissions",
    "update_permission_rules, set_default_permission_mode, or update_workspace_directories \
     (depending on what you're changing)",
)];

/// Map a wire-writable destination to its file layer. Narrower than
/// [`SettingsLayer`]'s full set by construction (`WritableScopeDto` has
/// no `Defaults` / `Cli` / `Managed` / `Env` variant), so this is infallible.
fn destination_layer(destination: WritableScopeDto) -> SettingsLayer {
    match destination {
        WritableScopeDto::User => SettingsLayer::User,
        WritableScopeDto::Project => SettingsLayer::Project,
        WritableScopeDto::Local => SettingsLayer::Local,
    }
}

// ── Permission command routing ──────────────────────────────────────────
//
// The three permission commands (`UpdatePermissionRules`,
// `SetDefaultPermissionMode`, `UpdateWorkspaceDirectories`) route to
// `permission::persist`'s writers rather than through [`apply_patch`] above —
// that writer already owns per-destination exclusive locks, atomic
// root-confined replacement, alias-normalizing de-duplication, and
// unknown-key preservation, and re-deriving any of that here would give the
// `permissions` key a second, competing write path (exactly what
// [`RESERVED_KEYS`] refuses on the generic patch). The functions below only
// translate wire DTOs into the `permission` crate's own types; they never
// touch a settings file directly.

/// Map a wire-writable destination to the `permission` crate's persistence
/// target. Narrower than [`permission::PermissionUpdateDestination`]'s full
/// set by construction (`WritableScopeDto` has no `Session` / `CliArg`
/// variant), so this is infallible — mirrors [`destination_layer`] above at
/// the `permission` crate's granularity.
#[must_use]
pub fn permission_destination(
    destination: WritableScopeDto,
) -> permission::PermissionUpdateDestination {
    match destination {
        WritableScopeDto::User => permission::PermissionUpdateDestination::UserSettings,
        WritableScopeDto::Project => permission::PermissionUpdateDestination::ProjectSettings,
        WritableScopeDto::Local => permission::PermissionUpdateDestination::LocalSettings,
    }
}

/// Map a writable destination to the [`permission::PermissionRuleSource`] a
/// rule created at that layer should carry, so a persisted rule's `source`
/// matches the file it actually landed in.
fn permission_rule_source(destination: WritableScopeDto) -> permission::PermissionRuleSource {
    match destination {
        WritableScopeDto::User => {
            permission::PermissionRuleSource::Settings(protocol::SettingsScope::User)
        }
        WritableScopeDto::Project => {
            permission::PermissionRuleSource::Settings(protocol::SettingsScope::Project)
        }
        WritableScopeDto::Local => {
            permission::PermissionRuleSource::Settings(protocol::SettingsScope::Local)
        }
    }
}

/// Map the wire-level rule-behavior bucket to `permission`'s own enum.
#[must_use]
pub fn permission_behavior(
    behavior: client_protocol::commands::PermissionBehaviorDto,
) -> permission::PermissionBehavior {
    match behavior {
        client_protocol::commands::PermissionBehaviorDto::Allow => {
            permission::PermissionBehavior::Allow
        }
        client_protocol::commands::PermissionBehaviorDto::Deny => {
            permission::PermissionBehavior::Deny
        }
        client_protocol::commands::PermissionBehaviorDto::Ask => {
            permission::PermissionBehavior::Ask
        }
    }
}

/// Build a [`permission::PermissionRule`] from one wire rule string. Parsing
/// is INFALLIBLE: [`permission::PermissionRuleValue::from_rule_string`]
/// degrades a malformed string to a bare tool name — 1:1 parity with
/// claude-code's own `permissionRuleValueFromString` — so there is no
/// rejected-input case to invent here. Whatever the caller typed becomes some
/// rule.
#[must_use]
pub fn permission_rule_from_wire(
    raw: &str,
    behavior: client_protocol::commands::PermissionBehaviorDto,
    destination: WritableScopeDto,
) -> permission::PermissionRule {
    permission::PermissionRule {
        value: permission::PermissionRuleValue::from_rule_string(raw),
        behavior: permission_behavior(behavior),
        source: permission_rule_source(destination),
    }
}

/// Resolve the `permission` crate's two-root [`permission::PermissionPaths`]
/// from [`SettingsPaths`]. `PermissionPaths::cwd` is `SettingsPaths::project_dir`
/// — the SAME root [`apply_patch`] already resolves the project/local file
/// layers from, not a second source of truth for it.
#[must_use]
pub fn permission_paths(paths: &SettingsPaths) -> permission::PermissionPaths {
    permission::PermissionPaths {
        lingxi_home: paths.lingxi_home.clone(),
        cwd: paths.project_dir.clone(),
    }
}

/// Apply a batch of shallow, top-level patches to one writable settings
/// layer. `None` deletes the key; unknown keys already in the file survive
/// verbatim (`read_settings_map`'s semantics — this reads the file, edits the
/// map in memory, and rewrites the whole thing).
///
/// The read-modify-write transaction is shared with
/// `migrations::settings_update::update_settings`; its callback lets this
/// crate mark the write for the desktop watcher immediately before publication
/// while `migrations` remains independent of `permission`.
///
/// # Errors
/// The destination resolves to a non-writable layer (unreachable given
/// [`WritableScopeDto`]'s three variants, but `writable_path` is the
/// single source of truth so its `Result` is still propagated rather than
/// unwrapped), the patch touches a [`RESERVED_KEYS`] key, the destination file
/// exists but is not valid JSON, or the write to disk fails. A broken
/// destination file is never overwritten: `read_settings_map` returns `Err`
/// before this function reaches the write.
pub fn apply_patch(
    paths: &SettingsPaths,
    destination: WritableScopeDto,
    patch: Vec<(String, Option<Value>)>,
) -> Result<(), String> {
    apply_patch_before_publish(paths, destination, patch, || {})
}

/// Internal test seam: invoke `before_publish` while the destination lock is
/// held, after parsing, merging, and serializing the patch but before its
/// atomic publication.
/// Keeping this seam private lets tests prove the lock covers the complete
/// read-modify-write transaction without widening the bridge API.
fn apply_patch_before_publish<F>(
    paths: &SettingsPaths,
    destination: WritableScopeDto,
    patch: Vec<(String, Option<Value>)>,
    before_publish: F,
) -> Result<(), String>
where
    F: FnOnce(),
{
    for (key, value) in &patch {
        if key == "providerRegion" {
            if let Some(value) = value {
                serde_json::from_value::<lingxi_core::settings::ProviderRegion>(value.clone())
                    .map_err(|_| {
                        "providerRegion must be international or china_mainland".to_owned()
                    })?;
            }
        }
        if let Some((reserved, replacement)) =
            RESERVED_KEYS.iter().find(|(reserved, _)| reserved == key)
        {
            return Err(format!(
                "the `{reserved}` key has a dedicated writer and is refused here; \
                 use {replacement} instead"
            ));
        }
    }

    let path = writable_path(paths, destination_layer(destination))?;
    migrations::settings_update::update_settings_with_before_publish(&path, patch, || {
        // I2: mark BEFORE writing. `settings_watch.rs` consumes this mark
        // within a 5-second window; skipping it makes the desktop's own
        // save look like an external edit and fires an unwanted hook round.
        permission::mark_internal_write(&path);
        before_publish();
    })
    .map_err(bridge_settings_error)
}

/// Keep the bridge's established protocol-facing wording while delegating the
/// transaction to the shared migrations writer. Read/parse errors already use
/// the same text; only the writer's operation labels need their historical
/// lowercase spelling restored here.
fn bridge_settings_error(error: String) -> String {
    for (shared, bridge) in [
        ("Failed to create ", "failed to create "),
        (
            "Failed to serialize settings for ",
            "failed to serialize settings for ",
        ),
        (
            "Failed to write settings to ",
            "failed to write settings to ",
        ),
    ] {
        if let Some(rest) = error.strip_prefix(shared) {
            return format!("{bridge}{rest}");
        }
    }
    error
}

/// The layer's name as it appears in the wire payloads
/// (`provenance_json`'s values and `files_json`'s `layer` field).
///
/// Spelled out rather than derived from `Debug` so a rename of the Rust
/// variant cannot silently change the wire contract — which is exactly what
/// keeps `Builtin` reporting as `"defaults"` here, the name this panel has
/// always used.
///
/// The last three rungs back no settings file and so never reach a snapshot;
/// they are named rather than lumped into a catch-all so that adding one later
/// is a deliberate edit here instead of a silent relabel.
#[must_use]
pub fn layer_wire_name(layer: SettingsLayer) -> &'static str {
    match layer {
        SettingsLayer::Builtin => "defaults",
        SettingsLayer::User => "user",
        SettingsLayer::Project => "project",
        SettingsLayer::Local => "local",
        SettingsLayer::Cli => "cli",
        SettingsLayer::Managed => "managed",
        SettingsLayer::Env => "env",
        SettingsLayer::Flag => "flag",
        SettingsLayer::Session => "session",
        SettingsLayer::Team => "team",
    }
}

/// Everything the router needs to answer the `Settings` listing, supplied once
/// by the composition root.
///
/// `active` and `managed` are inputs rather than something this crate computes:
/// `active` is the file layers PLUS the managed overlay, as loaded at session
/// start (see [`SettingsSnapshot::active`] for why managed is folded in), and
/// `managed` comes from the managed-settings layers the desktop composition
/// root already loads (`harness_runtime::desktop::managed_settings_overlay`).
/// bridge-server deliberately does not re-implement managed-layer loading.
pub struct SettingsContext {
    /// The two roots every file layer's path is resolved from.
    pub paths: SettingsPaths,
    /// The values this session had loaded at session start — the three file
    /// layers PLUS the managed overlay, but NOT `cli` or `env`. See
    /// [`SettingsSnapshot::active`] for why managed is included: the engine
    /// loads it at startup too, so leaving it out would make a policy-pinned
    /// key look eternally "pending a restart" that no restart could apply.
    pub active: Arc<RwLock<BTreeMap<String, Value>>>,
    /// The administrator's managed (policy) overlay, key → value. Its values
    /// win over every file layer, and its keys ARE the locked set reported on
    /// the wire.
    pub managed: BTreeMap<String, Value>,
}

impl SettingsContext {
    /// Snapshot the active baseline without exposing the synchronization
    /// primitive to protocol code. A poisoned lock degrades to the last inner
    /// value rather than turning a settings listing into a process failure.
    #[must_use]
    pub fn active_snapshot(&self) -> BTreeMap<String, Value> {
        self.active
            .read()
            .map(|active| active.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Mark a set of top-level settings keys as applied by a successful atomic
    /// hot reload. Values come from a freshly merged disk snapshot; keys that
    /// disappeared are removed so a lower layer can become the active value.
    pub fn mark_keys_applied(&self, keys: &[&str]) {
        let current = self.active_snapshot();
        let effective = build_snapshot(&self.paths, current, self.managed.clone()).effective;
        let mut active = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in keys {
            match effective.get(*key) {
                Some(value) => {
                    active.insert((*key).to_string(), value.clone());
                }
                None => {
                    active.remove(*key);
                }
            }
        }
    }
}

/// A [`SettingsSnapshot`] lowered to the wire's payloads.
///
/// Every structured field is a JSON **String**: `serde_json::Value` must not
/// enter `client-protocol` (decision §0.4 — it is not UniFFI-representable),
/// so structured payloads travel as strings exactly the way
/// `ToolUseStarted.input_json` does. `locked` stays a plain string list, which
/// IS representable.
pub struct LoweredSettings {
    /// `{key: value}` — the merged effective settings.
    pub effective_json: String,
    /// `{key: layer}` — which layer each effective value came from.
    pub provenance_json: String,
    /// `[{layer, path, exists, parsed, parse_error?}]` — one entry per file
    /// layer, in the module's lowest-priority-first order. The flag is named
    /// `parsed`, not `writable`: it reports whether the file's JSON parsed,
    /// which says nothing about OS write permission.
    pub files_json: String,
    /// `{key: value}` — the file-layer values as read at session start.
    pub active_json: String,
    /// Administrator-locked keys, passed through unchanged.
    pub locked: Vec<String>,
    /// Keys whose effective value is a cross-layer union, passed through
    /// unchanged. See [`SettingsSnapshot::merged_keys`] — a client must not
    /// render a single-layer provenance badge for one of these.
    pub merged_keys: Vec<String>,
    /// `{layer: {key: value}}` — each file layer's own raw map, unmerged.
    /// See [`SettingsSnapshot::layers`] for why a layered editor needs this
    /// instead of `effective_json` before writing back to one layer.
    pub layers_json: String,
}

/// Lower a snapshot to the wire payloads. A pure function called by the
/// router, matching how the router lowers every other reply through the pure
/// `client_adapter::lowering` fns rather than lowering inside the builder.
///
/// Serialization of a `BTreeMap<String, Value>` and of the file list cannot
/// fail (both are plain JSON objects/arrays with string keys), but a panic at
/// the routing seam is never acceptable, so a failure degrades to an empty
/// JSON document rather than unwrapping.
#[must_use]
pub fn lower_snapshot(snapshot: &SettingsSnapshot) -> LoweredSettings {
    let provenance: BTreeMap<&str, &str> = snapshot
        .provenance
        .iter()
        .map(|(key, layer)| (key.as_str(), layer_wire_name(*layer)))
        .collect();
    let files: Vec<Value> = snapshot
        .files
        .iter()
        .map(|file| {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "layer".to_string(),
                Value::from(layer_wire_name(file.layer)),
            );
            entry.insert(
                "path".to_string(),
                Value::from(file.path.to_string_lossy().into_owned()),
            );
            entry.insert("exists".to_string(), Value::from(file.exists));
            // `SettingsFile::writable` is `parse_error.is_none()` — it reports
            // whether the JSON parsed, NOT whether the OS permits writing (a
            // chmod-444 file parses fine). The wire says what it means.
            entry.insert("parsed".to_string(), Value::from(file.writable));
            if let Some(error) = &file.parse_error {
                entry.insert("parse_error".to_string(), Value::from(error.clone()));
            }
            Value::Object(entry)
        })
        .collect();

    LoweredSettings {
        effective_json: to_json_or_empty_object("effective", &snapshot.effective),
        provenance_json: to_json_or_empty_object("provenance", &provenance),
        files_json: serde_json::to_string(&files).unwrap_or_else(|error| {
            tracing::warn!(
                %error,
                "bridge-server: settings file layers failed to serialize; \
                 the client is told no settings files exist"
            );
            "[]".to_string()
        }),
        active_json: to_json_or_empty_object("active", &snapshot.active),
        locked: snapshot.locked.clone(),
        merged_keys: snapshot.merged_keys.clone(),
        layers_json: to_json_or_empty_object("layers", &snapshot.layers),
    }
}

/// Serialize a map, degrading to `{}` rather than panicking at the routing
/// seam. (Unreachable in practice: these maps are string-keyed JSON values.)
///
/// The degraded value is indistinguishable from "there are no settings", so it
/// is logged: a client that silently receives `{}` would show an empty settings
/// screen with nothing anywhere saying why.
fn to_json_or_empty_object<T: serde::Serialize>(what: &str, value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|error| {
        tracing::warn!(
            %error,
            what,
            "bridge-server: settings payload failed to serialize; \
             the client is told this payload is empty"
        );
        "{}".to_string()
    })
}

#[cfg(test)]
mod region_tests {
    use super::*;
    #[test]
    fn default_region_matches_runtime_without_a_spurious_restart() {
        let root = tempfile::tempdir().unwrap();
        let paths = SettingsPaths {
            lingxi_home: root.path().join("home"),
            project_dir: root.path().join("project"),
        };
        let active =
            BTreeMap::from([("providerRegion".into(), serde_json::json!("international"))]);
        let snapshot =
            build_snapshot_with_env(&paths, active.clone(), BTreeMap::new(), &BTreeMap::new());
        assert_eq!(
            snapshot.effective.get("providerRegion"),
            active.get("providerRegion")
        );
    }

    #[test]
    fn region_env_uses_core_validation_and_preserves_file_values_when_invalid() {
        let root = tempfile::tempdir().unwrap();
        let paths = SettingsPaths {
            lingxi_home: root.path().join("home"),
            project_dir: root.path().join("project"),
        };
        std::fs::create_dir_all(&paths.lingxi_home).unwrap();
        std::fs::write(
            paths.lingxi_home.join("settings.json"),
            r#"{"providerRegion":"china_mainland"}"#,
        )
        .unwrap();

        for (value, expected, layer) in [
            ("international", "international", SettingsLayer::Env),
            ("china_mainland", "china_mainland", SettingsLayer::Env),
            ("invalid-region", "china_mainland", SettingsLayer::User),
        ] {
            let env = BTreeMap::from([(
                format!("{}PROVIDER_REGION", env_parser::PREFIX_PRIORITY[0]),
                value.into(),
            )]);
            let snapshot = build_snapshot_with_env(&paths, BTreeMap::new(), BTreeMap::new(), &env);
            assert_eq!(
                snapshot.effective.get("providerRegion"),
                Some(&serde_json::json!(expected)),
                "{value}"
            );
            assert_eq!(snapshot.provenance.get("providerRegion"), Some(&layer));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Four layers could each define the same key; provenance must point at
    /// the layer the priority order actually produced, not a hardcoded
    /// string. Priority (highest first among file layers): local > project >
    /// user. If `FILE_LAYERS` were reversed this assertion would fail,
    /// because "from-local" would no longer be the value that wins the merge.
    #[test]
    fn provenance_names_the_layer_the_merged_value_actually_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();

        std::fs::write(home.join("settings.json"), r#"{"outputStyle":"from-user"}"#).unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.json"),
            r#"{"outputStyle":"from-project"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.local.json"),
            r#"{"outputStyle":"from-local"}"#,
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let active = BTreeMap::new();
        // A managed key that no file layer defines, so it exercises the locked
        // set without interfering with the precedence assertion below.
        let mut managed = BTreeMap::new();
        managed.insert("telemetryEnabled".to_string(), Value::from(false));
        let snap = build_snapshot(&paths, active, managed);

        assert_eq!(
            snap.effective.get("outputStyle").and_then(|v| v.as_str()),
            Some("from-local"),
            "local must win over project and user"
        );
        assert_eq!(
            snap.provenance.get("outputStyle"),
            Some(&SettingsLayer::Local),
            "provenance must name Local, the layer the winning value came from"
        );
        assert_eq!(
            snap.locked,
            vec!["telemetryEnabled".to_string()],
            "locked must be derived from the caller's managed overlay, not hardcoded"
        );
    }

    /// `layers` must give each layer its OWN raw value for an object-valued
    /// key like `providers` — never the cross-layer merged (`effective`)
    /// view. This is the exact bug a layered editor hit: pre-merging a write
    /// against `effective` for `providers` silently forked whichever OTHER
    /// layer's entries `effective` happened to be showing into the layer
    /// actually being saved, because `update_settings` replaces a key
    /// WHOLESALE in one layer's file rather than deep-merging
    /// (`migrations/src/settings_update.rs`).
    ///
    /// This test is written so that if `layers` were populated from
    /// `effective` (cloned once per layer) instead of from each layer's own
    /// raw map, it fails: `effective["providers"]` resolves to whichever
    /// layer wins the (flat, last-layer-wins) merge for that key — here,
    /// `local`, since `FILE_LAYERS` applies user → project → local in
    /// increasing priority — so a merged-view bug would make
    /// `layers["user"]` and `layers["project"]` both wrongly report the
    /// LOCAL layer's `providers` value instead of their own (or none, for
    /// `routing`, which only `project` defines).
    #[test]
    fn layers_gives_each_layer_its_own_raw_value_not_the_merged_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();

        std::fs::write(
            home.join("settings.json"),
            r#"{"providers":{"userProvider":{"type":"openai","models":[{"id":"m-user"}]}}}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.json"),
            r#"{"providers":{"projectProvider":{"type":"openai","models":[{"id":"m-project"}]}},"routing":{"retry":{"maxAttempts":3}}}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.local.json"),
            r#"{"providers":{"localProvider":{"type":"openai","models":[{"id":"m-local"}]}}}"#,
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let snap = build_snapshot(&paths, BTreeMap::new(), BTreeMap::new());

        // Sanity check on the premise: `effective` is the engine's merge, and
        // `providers` is one of the keys the engine DEEP-MERGES, so
        // `effective` shows all three layers' profiles at once and matches no
        // single layer's own map. That is what makes the assertions below
        // meaningful — if `layers` secretly reused `effective`, every layer
        // would report all three profiles. (Before Task 17b this premise read
        // the other way round: the snapshot's merge was flat, so `effective`
        // was local's own value. Either way the premise is "effective differs
        // from each layer's own map"; the union is the stronger version, and
        // it is the one the running engine actually resolves.)
        let effective_providers = snap.effective.get("providers").unwrap();
        assert!(
            effective_providers.get("userProvider").is_some()
                && effective_providers.get("projectProvider").is_some()
                && effective_providers.get("localProvider").is_some(),
            "premise check: effective must be the cross-layer union — got {effective_providers}"
        );
        assert!(
            snap.merged_keys.contains(&"providers".to_string()),
            "a union must be reported as merged, or the UI badges it as one layer's — got {:?}",
            snap.merged_keys
        );
        // `routing` is set by ONE layer only, so nothing was unioned for it —
        // its `project` badge is honest and must not be suppressed.
        assert!(
            !snap.merged_keys.contains(&"routing".to_string()),
            "a key only one layer defines is not a union, got {:?}",
            snap.merged_keys
        );

        let user_providers = snap.layers.get("user").unwrap().get("providers").unwrap();
        assert!(
            user_providers.get("userProvider").is_some(),
            "the user layer's own map must have userProvider, got {user_providers}"
        );
        assert!(
            user_providers.get("localProvider").is_none()
                && user_providers.get("projectProvider").is_none(),
            "the user layer's own map must NOT carry another layer's entries, got {user_providers}"
        );

        let project_providers = snap
            .layers
            .get("project")
            .unwrap()
            .get("providers")
            .unwrap();
        assert!(
            project_providers.get("projectProvider").is_some()
                && project_providers.get("userProvider").is_none()
                && project_providers.get("localProvider").is_none(),
            "the project layer's own map must be exactly its own value, got {project_providers}"
        );

        let local_providers = snap.layers.get("local").unwrap().get("providers").unwrap();
        assert!(
            local_providers.get("localProvider").is_some()
                && local_providers.get("userProvider").is_none()
                && local_providers.get("projectProvider").is_none(),
            "the local layer's own map must be exactly its own value, got {local_providers}"
        );

        // `routing` is defined ONLY at `project` — a merged-view bug would
        // make every layer report it (or none would, depending on how the
        // bug shaped up), so a layer that never wrote the key must have no
        // entry for it at all, not an empty object standing in for "unset".
        assert!(
            snap.layers.get("user").unwrap().get("routing").is_none(),
            "a layer that never set `routing` must have no entry for it"
        );
        assert!(
            snap.layers.get("local").unwrap().get("routing").is_none(),
            "a layer that never set `routing` must have no entry for it"
        );
        assert!(
            snap.layers.get("project").unwrap().get("routing").is_some(),
            "the layer that DID set `routing` must report it"
        );

        // The wire lowering must carry the same per-layer isolation through
        // `layers_json`'s JSON serialization.
        let lowered = lower_snapshot(&snap);
        let layers_wire: Value = serde_json::from_str(&lowered.layers_json).unwrap();
        assert!(
            layers_wire["user"]["providers"]["userProvider"].is_object(),
            "wire layers_json must keep the user layer's own providers, got {layers_wire}"
        );
        assert!(
            layers_wire["user"]["providers"]["localProvider"].is_null(),
            "wire layers_json must not leak another layer's entries into this one, got {layers_wire}"
        );
        assert!(
            layers_wire["project"]["routing"]["retry"]["maxAttempts"] == 3,
            "wire layers_json must carry the project layer's own routing value, got {layers_wire}"
        );
    }

    /// The engine does NOT resolve `hooks` (or any other key in
    /// `lingxi_core::settings::schema::MERGE_STRATEGIES`) by letting the
    /// highest-priority layer's whole value win: `merger::merge` deep-merges
    /// it, so a hook defined only in `user` survives alongside a hook defined
    /// only in `project`. A snapshot that overwrites the key top-level shows
    /// a settings UI a value the running engine never resolves.
    ///
    /// Written to fail against the flat, last-layer-wins merge: with it,
    /// `effective["hooks"]` is exactly the project layer's object, so
    /// `PreToolUse` (user-only) is absent.
    #[test]
    fn effective_unions_a_deep_merged_key_across_layers_the_way_the_engine_does() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();

        std::fs::write(
            home.join("settings.json"),
            r#"{"hooks":{"PreToolUse":{"Bash":"from-user"}},"outputStyle":"from-user"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.json"),
            r#"{"hooks":{"PostToolUse":{"Read":"from-project"}},"outputStyle":"from-project"}"#,
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let snap = build_snapshot(&paths, BTreeMap::new(), BTreeMap::new());

        let hooks = snap
            .effective
            .get("hooks")
            .expect("both layers define `hooks`");
        assert_eq!(
            hooks.pointer("/PreToolUse/Bash").and_then(|v| v.as_str()),
            Some("from-user"),
            "the user layer's hook must survive the merge with the project layer, got {hooks}"
        );
        assert_eq!(
            hooks.pointer("/PostToolUse/Read").and_then(|v| v.as_str()),
            Some("from-project"),
            "the project layer's hook must be present too, got {hooks}"
        );

        // A scalar key set in both layers is NOT a union — the engine's
        // default strategy is Override, so the higher layer simply wins.
        assert_eq!(
            snap.effective.get("outputStyle").and_then(|v| v.as_str()),
            Some("from-project"),
            "a scalar key must still be last-layer-wins"
        );

        // `merged_keys` must name the unioned key so the UI can drop the
        // single-layer badge for it...
        assert!(
            snap.merged_keys.contains(&"hooks".to_string()),
            "`hooks` resolved from two layers at once and must be reported as merged, got {:?}",
            snap.merged_keys
        );
        // ...and must NOT name the scalar key, which really does come from
        // one layer. A `merged_keys` that listed every key both layers
        // mention would satisfy the assertion above while telling the UI
        // nothing, so this half is what makes the field worth having.
        assert!(
            !snap.merged_keys.contains(&"outputStyle".to_string()),
            "`outputStyle` is scalar-override — its effective value IS the project layer's, so \
             suppressing that layer's badge would be the same lie inverted, got {:?}",
            snap.merged_keys
        );

        // The provenance entry for a merged key names only the highest
        // CONTRIBUTOR, which is exactly why `merged_keys` has to exist: on
        // its own this entry would have the UI say the value came from
        // `project` when half of it came from `user`.
        assert_eq!(
            snap.provenance.get("hooks"),
            Some(&SettingsLayer::Project),
            "provenance still names the top contributing layer"
        );

        // The wire carries it, or none of the above reaches the UI.
        let lowered = lower_snapshot(&snap);
        assert!(
            lowered.merged_keys.contains(&"hooks".to_string()),
            "the lowered payload must carry the merged keys, got {:?}",
            lowered.merged_keys
        );
        let effective_wire: Value = serde_json::from_str(&lowered.effective_json).unwrap();
        assert_eq!(
            effective_wire
                .pointer("/hooks/PreToolUse/Bash")
                .and_then(|v| v.as_str()),
            Some("from-user"),
            "the union must survive the wire lowering, got {effective_wire}"
        );
    }

    /// A key an earlier pair of layers unioned, which a later layer then
    /// redefines ENTIRELY, is no longer a union: the effective value is
    /// exactly that later layer's own value, so its badge is honest and must
    /// come back. `merged_keys` reports the state after the LAST layer to
    /// touch the key, not "some pair of layers once merged this".
    ///
    /// Fails against an accumulate-only `merged_keys`: `hooks` would stay in
    /// the list forever after the user/project fold unioned it, and the UI
    /// would suppress a true `local` badge.
    #[test]
    fn a_union_a_later_layer_fully_redefines_stops_being_reported_as_merged() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();

        std::fs::write(home.join("settings.json"), r#"{"hooks":{"A":"user"}}"#).unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.json"),
            r#"{"hooks":{"B":"project"}}"#,
        )
        .unwrap();
        // Defines BOTH entries, so nothing of the lower layers survives.
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.local.json"),
            r#"{"hooks":{"A":"local","B":"local"}}"#,
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let snap = build_snapshot(&paths, BTreeMap::new(), BTreeMap::new());

        assert_eq!(
            snap.effective.get("hooks"),
            Some(&serde_json::json!({"A": "local", "B": "local"})),
            "the local layer redefines every entry, so the merge lands on its own value"
        );
        assert_eq!(snap.provenance.get("hooks"), Some(&SettingsLayer::Local));
        assert!(
            !snap.merged_keys.contains(&"hooks".to_string()),
            "the effective value IS the local layer's own, so its badge is honest and must not \
             be suppressed, got {:?}",
            snap.merged_keys
        );
    }

    /// A layer that writes `"key": null` has not set that key: every
    /// `SettingsJson` field is an `Option`, so the engine parses it to `None`
    /// and the lower layer stands. The snapshot must agree — in `effective`
    /// AND in `provenance`, which would otherwise name a layer as the source
    /// of a value that layer does not supply.
    ///
    /// Fails against a raw merge that writes nulls through: `outputStyle`
    /// would read `null` with provenance `Local`, and `model` would exist as
    /// a null-valued key the engine never resolves.
    #[test]
    fn a_null_in_a_layer_is_unset_and_never_becomes_that_layers_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();

        std::fs::write(home.join("settings.json"), r#"{"outputStyle":"from-user"}"#).unwrap();
        // `outputStyle` null does not erase the user layer; `model` null is
        // set by no other layer, so it stays absent entirely.
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.local.json"),
            r#"{"outputStyle":null,"model":null}"#,
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let snap = build_snapshot(&paths, BTreeMap::new(), BTreeMap::new());

        assert_eq!(
            snap.effective.get("outputStyle").and_then(|v| v.as_str()),
            Some("from-user"),
            "a null in the local layer must not erase the user layer's value"
        );
        assert_eq!(
            snap.provenance.get("outputStyle"),
            Some(&SettingsLayer::User),
            "provenance must name the layer that actually supplies the value"
        );
        assert_eq!(
            snap.effective.get("model"),
            None,
            "a key only ever written as null is unset, not present-and-null, got {:?}",
            snap.effective.get("model")
        );
        assert_eq!(
            snap.provenance.get("model"),
            None,
            "no layer sets `model`, so no layer may be named as its source"
        );
        // The raw per-layer view is untouched: it reports what the FILE says,
        // nulls included, because that is the thing a layered editor writes
        // back against.
        assert_eq!(
            snap.layers.get("local").unwrap().get("model"),
            Some(&Value::Null),
            "`layers` must still show the file's own contents verbatim"
        );
    }

    /// The managed (policy) overlay is not exempt from the merge: the engine
    /// folds it in with `merger::merge(acc, managed)` like every other layer,
    /// so a pinned key it deep-merges resolves to the policy's entries UNIONED
    /// with the file layers'. Applying the overlay as a flat overwrite here —
    /// which is what this function used to do — would report a value the
    /// engine never resolves for exactly the keys the UI draws a padlock next
    /// to, and would claim the whole value is the administrator's.
    ///
    /// Fails against a flat overlay: `allow` (file-only) disappears.
    #[test]
    fn the_managed_overlay_deep_merges_the_way_the_engine_folds_it_in() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"permissions":{"allow":["Bash(ls)"]}}"#,
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let mut managed = BTreeMap::new();
        managed.insert(
            "permissions".to_string(),
            serde_json::json!({"deny": ["Bash(rm -rf /)"]}),
        );
        let snap = build_snapshot(&paths, BTreeMap::new(), managed);

        let permissions = snap.effective.get("permissions").unwrap();
        assert_eq!(
            permissions.pointer("/deny/0").and_then(|v| v.as_str()),
            Some("Bash(rm -rf /)"),
            "the administrator's deny rule must be there, got {permissions}"
        );
        assert_eq!(
            permissions.pointer("/allow/0").and_then(|v| v.as_str()),
            Some("Bash(ls)"),
            "the file layer's allow rule must survive the managed overlay, got {permissions}"
        );
        assert!(
            snap.merged_keys.contains(&"permissions".to_string()),
            "the value is a union of policy and file, got {:?}",
            snap.merged_keys
        );
        assert_eq!(
            snap.locked,
            vec!["permissions".to_string()],
            "the key is still administrator-pinned"
        );
    }

    /// A key an administrator pinned through the managed layer must resolve
    /// to the MANAGED value with provenance `Managed`, even though a file
    /// layer also defines it. Managed outranks every file layer, so reporting
    /// the file's value here would show a settings UI the wrong current value
    /// for exactly the keys it draws a padlock next to.
    ///
    /// This fails if the overlay is applied BEFORE the file layers instead of
    /// after: `outputStyle` would then read `"from-local"` with provenance
    /// `Local`. The un-pinned key proves the overlay does not clobber the
    /// file merge wholesale.
    #[test]
    fn the_managed_overlay_wins_over_every_file_layer() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"outputStyle":"from-user","model":"from-user"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.local.json"),
            r#"{"outputStyle":"from-local"}"#,
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let mut managed = BTreeMap::new();
        managed.insert("outputStyle".to_string(), Value::from("from-managed"));
        let snap = build_snapshot(&paths, BTreeMap::new(), managed);

        assert_eq!(
            snap.effective.get("outputStyle").and_then(|v| v.as_str()),
            Some("from-managed"),
            "the managed overlay must beat the local file layer that also sets this key"
        );
        assert_eq!(
            snap.provenance.get("outputStyle"),
            Some(&SettingsLayer::Managed),
            "provenance must name Managed for a key the administrator pinned"
        );
        assert_eq!(
            snap.locked,
            vec!["outputStyle".to_string()],
            "the locked set is exactly the overlay's keys"
        );
        // A key the overlay does not mention keeps its file-layer resolution.
        assert_eq!(
            snap.effective.get("model").and_then(|v| v.as_str()),
            Some("from-user")
        );
        assert_eq!(snap.provenance.get("model"), Some(&SettingsLayer::User));
    }

    /// A managed-overlay key must not appear in the difference between
    /// `effective` and `active` — never permanently, and this is the whole
    /// point of `active_settings_baseline`. `effective` re-applies the same
    /// managed overlay on every later listing; if `active` (captured once at
    /// boot) leaves it out, a policy-pinned key differs from `effective`
    /// forever, and `pendingKeys` (the desktop shell's TypeScript diff, see
    /// `clients/electron/.../useEngineSettings.ts`) reports a "restart to
    /// apply" banner for a change no restart can ever resolve, since the user
    /// never wrote it and no restart changes it.
    ///
    /// This mirrors exactly what `boot.rs::assemble_with_provider_keys` does
    /// (`active_settings_baseline` for `SettingsContext.active`, then
    /// `build_snapshot` again at listing time with that same `active` and
    /// `managed`) and exactly what a REGRESSION back to the old
    /// `build_snapshot(&paths, BTreeMap::new(), BTreeMap::new()).effective`
    /// (empty managed) would break: with that old call, `active` would carry
    /// no `outputStyle` key at all, so the `assert_eq!` below on `active`
    /// would fail immediately, and the `snap.effective`/`snap.active`
    /// equality would fail too (`Some("from-managed") != None`).
    #[test]
    fn a_managed_overlay_key_never_differs_between_effective_and_active() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();
        // A file layer also sets this key, so the test cannot pass by
        // accident (a managed key with no file competitor would trivially
        // agree even with the old, buggy empty-managed baseline: `None ==
        // None` when re-merged with an empty overlay in `active`'s own
        // build_snapshot call below — this needs a REAL value on both sides).
        std::fs::write(home.join("settings.json"), r#"{"outputStyle":"from-user"}"#).unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let mut managed = BTreeMap::new();
        managed.insert("outputStyle".to_string(), Value::from("from-managed"));

        // Exactly what `boot.rs` does once, at connection setup.
        let active = active_settings_baseline(&paths, &managed);
        assert_eq!(
            active.get("outputStyle").and_then(|v| v.as_str()),
            Some("from-managed"),
            "active must already carry the managed value, or it can never agree with effective",
        );

        // Exactly what `emit_settings_snapshot` does on every later listing.
        let snap = build_snapshot(&paths, active, managed);
        assert_eq!(
            snap.effective.get("outputStyle"),
            snap.active.get("outputStyle"),
            "a managed key must be identical between effective and active — nothing is pending \
             on a key the user cannot change",
        );
    }

    /// Only the layers a user session could actually write are writable
    /// destinations. Reject everything else, naming both the rejected value
    /// and the writable set so the error is actionable.
    #[test]
    fn writable_path_rejects_non_writable_destinations() {
        let paths = SettingsPaths {
            lingxi_home: "/home/u/.lingxi".into(),
            project_dir: "/work/repo".into(),
        };
        let err = writable_path(&paths, SettingsLayer::Managed).unwrap_err();
        assert!(
            err.to_lowercase().contains("managed"),
            "error must name the rejected value, got: {err}"
        );
        assert!(
            err.to_lowercase().contains("user")
                && err.to_lowercase().contains("project")
                && err.to_lowercase().contains("local"),
            "error must name the writable set, got: {err}"
        );
    }

    /// The three file layers must each resolve to their real on-disk path
    /// when writable.
    #[test]
    fn writable_path_resolves_the_three_writable_layers() {
        let home: std::path::PathBuf = "/home/u/.lingxi".into();
        let project: std::path::PathBuf = "/work/repo".into();
        let paths = SettingsPaths {
            lingxi_home: home.clone(),
            project_dir: project.clone(),
        };

        assert_eq!(
            writable_path(&paths, SettingsLayer::User).unwrap(),
            home.join("settings.json")
        );
        assert_eq!(
            writable_path(&paths, SettingsLayer::Project).unwrap(),
            project.join(branding::DOT_DIR).join("settings.json")
        );
        assert_eq!(
            writable_path(&paths, SettingsLayer::Local).unwrap(),
            project.join(branding::DOT_DIR).join("settings.local.json")
        );
    }

    /// The lowering must keep `active` DISTINCT from `effective` — that
    /// distinction is the whole point of the field: an on-disk edit that the
    /// running session has not adopted shows up as the two disagreeing. It
    /// must also name each layer with its wire spelling and carry a parse
    /// error through to the file entry.
    #[test]
    fn lowering_keeps_active_distinct_from_effective_and_names_each_layer() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();
        std::fs::write(home.join("settings.json"), r#"{"model":"on-disk"}"#).unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.local.json"),
            "{ not json",
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let mut active = BTreeMap::new();
        active.insert("model".to_string(), Value::from("loaded-at-startup"));
        let lowered = lower_snapshot(&build_snapshot(&paths, active, BTreeMap::new()));

        let effective: Value = serde_json::from_str(&lowered.effective_json).unwrap();
        let active: Value = serde_json::from_str(&lowered.active_json).unwrap();
        assert_eq!(effective["model"], "on-disk");
        assert_eq!(
            active["model"], "loaded-at-startup",
            "active must report what the session loaded, not re-report the files"
        );

        let provenance: Value = serde_json::from_str(&lowered.provenance_json).unwrap();
        assert_eq!(
            provenance["model"], "user",
            "the wire spelling of the layer, lowercase, not the Rust Debug name"
        );

        let files: Value = serde_json::from_str(&lowered.files_json).unwrap();
        let broken = files
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["layer"] == "local")
            .expect("the local layer must be reported even when it fails to parse");
        assert_eq!(
            broken["parsed"], false,
            "the wire flag is `parsed` — it reports JSON validity, not OS write permission"
        );
        assert!(
            broken["parse_error"].is_string(),
            "the parse error must reach the wire, got {broken}"
        );
    }

    /// A file layer with syntactically broken JSON must not silently drop
    /// out of the snapshot: it is reported as existing but not writable, with
    /// the parse error preserved, and it must not clobber the keys the other
    /// layers contributed.
    #[test]
    fn broken_layer_reports_parse_error_and_is_not_writable() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(branding::DOT_DIR)).unwrap();

        std::fs::write(home.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
        std::fs::write(
            project.join(branding::DOT_DIR).join("settings.json"),
            "{ not json",
        )
        .unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let snap = build_snapshot(&paths, BTreeMap::new(), BTreeMap::new());

        let project_file = snap
            .files
            .iter()
            .find(|f| f.layer == SettingsLayer::Project)
            .unwrap();
        assert!(project_file.exists);
        assert!(!project_file.writable);
        assert!(project_file.parse_error.is_some());
        // The user layer's key must still be present in the merge.
        assert_eq!(
            snap.effective.get("model").and_then(|v| v.as_str()),
            Some("opus")
        );
    }

    /// I1: the generic patch must not write `permissions`. The patch is a
    /// SHALLOW, top-level replace of the whole object, so at this point there
    /// is no way to tell which nested field the caller actually meant
    /// (`allow`/`deny`/`ask`, `defaultMode`, or `additionalDirectories`) — the
    /// error must therefore name the reserved key AND ALL THREE replacement
    /// commands, not just one, or a caller who meant `defaultMode` /
    /// `additionalDirectories` would be pointed at the wrong command.
    #[test]
    fn generic_patch_refuses_the_permissions_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SettingsPaths {
            lingxi_home: dir.path().join("home"),
            project_dir: dir.path().join("repo"),
        };
        let err = apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("permissions".to_string(), Some(serde_json::json!({})))],
        )
        .unwrap_err();
        assert!(
            err.contains("permissions"),
            "error must name the key, got: {err}"
        );
        assert!(
            err.contains("update_permission_rules"),
            "error must name the rule-set replacement command, got: {err}"
        );
        assert!(
            err.contains("set_default_permission_mode"),
            "error must name the default-mode replacement command, got: {err}"
        );
        assert!(
            err.contains("update_workspace_directories"),
            "error must name the workspace-directories replacement command, got: {err}"
        );
    }

    /// I2: a write must leave an internal-write mark, or the desktop's own
    /// save gets misread by the watcher as an external edit.
    #[test]
    fn a_write_leaves_an_internal_write_mark() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SettingsPaths {
            lingxi_home: dir.path().join("home"),
            project_dir: dir.path().join("repo"),
        };
        apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("outputStyle".to_string(), Some(serde_json::json!("terse")))],
        )
        .unwrap();
        let path = writable_path(&paths, SettingsLayer::User).unwrap();
        assert!(
            permission::consume_internal_write(&path, std::time::Duration::from_secs(5)),
            "apply_patch must call mark_internal_write before writing {}",
            path.display()
        );
    }

    /// I2's negative control: proves the assertion above can actually fail.
    /// An unmarked write must make `consume_internal_write` return `false` —
    /// otherwise that gate would be permanently green while proving nothing.
    #[test]
    fn the_internal_write_assertion_can_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unmarked.json");
        std::fs::write(&path, "{}\n").unwrap();
        assert!(
            !permission::consume_internal_write(&path, std::time::Duration::from_secs(5)),
            "an unmarked write must NOT be consumable; if this passes, the gate in \
             a_write_leaves_an_internal_write_mark proves nothing"
        );
    }

    /// A patch touching one key must leave every OTHER key in the destination
    /// file exactly as it was. The other three `apply_patch` tests all write
    /// into a brand-new empty tempdir, so none of them can observe this: they
    /// would pass identically if `apply_patch` silently dropped unrelated
    /// keys. This seeds the file with unrelated content first.
    #[test]
    fn a_patch_leaves_unrelated_keys_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SettingsPaths {
            lingxi_home: dir.path().join("home"),
            project_dir: dir.path().join("repo"),
        };
        let path = writable_path(&paths, SettingsLayer::User).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"model":"opus","unknownVendorKey":"x"}"#).unwrap();

        apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("outputStyle".to_string(), Some(serde_json::json!("terse")))],
        )
        .unwrap();

        let map = read_settings_map(&path).unwrap();
        assert_eq!(
            map.get("model"),
            Some(&serde_json::json!("opus")),
            "a key untouched by the patch must survive with its original value"
        );
        assert_eq!(
            map.get("unknownVendorKey"),
            Some(&serde_json::json!("x")),
            "a key this crate does not even know the meaning of must still survive verbatim"
        );
        assert_eq!(
            map.get("outputStyle"),
            Some(&serde_json::json!("terse")),
            "the patched key must also be applied alongside the untouched ones"
        );
    }

    /// The lock must cover the read as well as the later publish. Holding the
    /// first patch before publication makes a concurrent second patch wait;
    /// once it proceeds, it reads the first patch's value and preserves both
    /// keys.
    #[test]
    fn concurrent_patches_to_one_path_preserve_both_keys() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        let path = home.join("settings.json");
        let paths = SettingsPaths {
            lingxi_home: home.clone(),
            project_dir: project.clone(),
        };
        apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("original".into(), Some(serde_json::json!(true)))],
        )
        .unwrap();
        let _ = permission::consume_internal_write(&path, Duration::from_secs(5));

        let (first_read_tx, first_read_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_paths = SettingsPaths {
            lingxi_home: home.clone(),
            project_dir: project.clone(),
        };
        let first = std::thread::spawn(move || {
            apply_patch_before_publish(
                &first_paths,
                WritableScopeDto::User,
                vec![("first".into(), Some(serde_json::json!(1)))],
                move || {
                    first_read_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
        });
        first_read_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first patch must reach before publication before the second starts");

        let (second_started_tx, second_started_rx) = mpsc::channel();
        let (second_read_tx, second_read_rx) = mpsc::channel();
        let second_paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let second = std::thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            apply_patch_before_publish(
                &second_paths,
                WritableScopeDto::User,
                vec![("second".into(), Some(serde_json::json!(2)))],
                move || second_read_tx.send(()).unwrap(),
            )
        });
        second_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second patch must start");
        let second_read_while_first_held = second_read_rx
            .recv_timeout(Duration::from_millis(100))
            .is_ok();
        release_tx.send(()).unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        assert!(
            !second_read_while_first_held,
            "same-path patch reached publication before the first RMW released the lock"
        );

        let map = read_settings_map(&path).unwrap();
        assert_eq!(map["original"], serde_json::json!(true));
        assert_eq!(map["first"], serde_json::json!(1));
        assert_eq!(map["second"], serde_json::json!(2));
    }

    /// A lock is keyed by the destination path, not global to the bridge. A
    /// blocked user-layer patch must not stall an independent project-layer
    /// patch.
    #[test]
    fn patches_to_different_paths_proceed_independently() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        let user_path = home.join("settings.json");
        let project_path = project.join(branding::DOT_DIR).join("settings.json");

        let (first_read_tx, first_read_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_paths = SettingsPaths {
            lingxi_home: home.clone(),
            project_dir: project.clone(),
        };
        let first = std::thread::spawn(move || {
            apply_patch_before_publish(
                &first_paths,
                WritableScopeDto::User,
                vec![("user".into(), Some(serde_json::json!(true)))],
                move || {
                    first_read_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
        });
        first_read_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("user patch must reach before publication");

        let (second_read_tx, second_read_rx) = mpsc::channel();
        let second_paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        let second = std::thread::spawn(move || {
            apply_patch_before_publish(
                &second_paths,
                WritableScopeDto::Project,
                vec![("project".into(), Some(serde_json::json!(true)))],
                move || second_read_tx.send(()).unwrap(),
            )
        });
        second_read_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("independent project patch must reach before publication immediately");
        second.join().unwrap().unwrap();
        release_tx.send(()).unwrap();
        first.join().unwrap().unwrap();

        assert_eq!(read_settings_map(&user_path).unwrap()["user"], true);
        assert_eq!(read_settings_map(&project_path).unwrap()["project"], true);
    }

    /// Broken JSON remains untouched and is returned as the same internal
    /// settings-update failure that the router wraps in `ClientEvent::Error`.
    #[test]
    fn broken_settings_file_is_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SettingsPaths {
            lingxi_home: dir.path().join("home"),
            project_dir: dir.path().join("repo"),
        };
        let path = writable_path(&paths, SettingsLayer::User).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = b"{ not valid json";
        std::fs::write(&path, original).unwrap();

        let error = apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("new".into(), Some(serde_json::json!(true)))],
        )
        .unwrap_err();
        assert!(error.to_lowercase().contains("json"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(settings_temps(&path).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn patch_preserves_mode_trailing_newline_and_temp_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = SettingsPaths {
            lingxi_home: dir.path().join("home"),
            project_dir: dir.path().join("repo"),
        };
        let path = writable_path(&paths, SettingsLayer::User).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"old\":true}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("new".into(), Some(serde_json::json!(true)))],
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert!(!bytes.ends_with(b"\n\n"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(settings_temps(&path).is_empty());
        let _ = permission::consume_internal_write(&path, Duration::from_secs(5));
    }

    /// Final settings-file links are part of the bridge's existing direct-write
    /// scope: follow the target and leave the link itself intact. This also
    /// proves staging happens beside the resolved target, not beside the link.
    #[cfg(unix)]
    #[test]
    fn patch_follows_final_symlink_target_without_replacing_link() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("settings.json");
        let link = home.join("settings.json");
        std::fs::write(&target, r#"{"keep":true}"#).unwrap();
        symlink(&target, &link).unwrap();

        let paths = SettingsPaths {
            lingxi_home: home,
            project_dir: project,
        };
        apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("patched".into(), Some(serde_json::json!(true)))],
        )
        .unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        let map = read_settings_map(&target).unwrap();
        assert_eq!(map["keep"], true);
        assert_eq!(map["patched"], true);
        assert!(settings_temps(&target).is_empty());
        let _ = permission::consume_internal_write(&link, Duration::from_secs(5));
    }

    /// Parent-directory links remain accepted by the direct settings path
    /// policy; the staged file lands in that directory's resolved target.
    #[cfg(unix)]
    #[test]
    fn patch_accepts_symlinked_settings_parent_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home_target = dir.path().join("home-target");
        let home_link = dir.path().join("home-link");
        std::fs::create_dir_all(&home_target).unwrap();
        symlink(&home_target, &home_link).unwrap();
        let paths = SettingsPaths {
            lingxi_home: home_link,
            project_dir: dir.path().join("repo"),
        };
        apply_patch(
            &paths,
            WritableScopeDto::User,
            vec![("value".into(), Some(serde_json::json!(1)))],
        )
        .unwrap();
        let target = home_target.join("settings.json");
        assert_eq!(read_settings_map(&target).unwrap()["value"], 1);
        assert!(settings_temps(&target).is_empty());
    }

    fn settings_temps(path: &std::path::Path) -> Vec<PathBuf> {
        let prefix = format!(
            "{}.tmp.",
            path.file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default()
        );
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect()
    }

    //
    // `persist_permission_rule_set` / `persist_permission_mode` /
    // `persist_workspace_directories` are exercised end-to-end (through the
    // router) in `router_test.rs`; the pure translation functions here — the
    // part genuinely new to this task rather than already covered by
    // `permission`'s own suite — get direct unit coverage.

    #[test]
    fn permission_destination_maps_every_writable_layer() {
        assert_eq!(
            permission_destination(WritableScopeDto::User),
            permission::PermissionUpdateDestination::UserSettings
        );
        assert_eq!(
            permission_destination(WritableScopeDto::Project),
            permission::PermissionUpdateDestination::ProjectSettings
        );
        assert_eq!(
            permission_destination(WritableScopeDto::Local),
            permission::PermissionUpdateDestination::LocalSettings
        );
    }

    #[test]
    fn permission_behavior_maps_every_bucket() {
        use client_protocol::commands::PermissionBehaviorDto;
        assert_eq!(
            permission_behavior(PermissionBehaviorDto::Allow),
            permission::PermissionBehavior::Allow
        );
        assert_eq!(
            permission_behavior(PermissionBehaviorDto::Deny),
            permission::PermissionBehavior::Deny
        );
        assert_eq!(
            permission_behavior(PermissionBehaviorDto::Ask),
            permission::PermissionBehavior::Ask
        );
    }

    /// A well-formed rule string parses to the tool/content split, and the
    /// resulting rule's `source` names the layer it will be written to — not
    /// some other layer — so a later `priority()` lookup on the persisted
    /// rule resolves correctly.
    #[test]
    fn permission_rule_from_wire_carries_the_destinations_source() {
        use client_protocol::commands::PermissionBehaviorDto;
        let rule = permission_rule_from_wire(
            "Bash(ls:*)",
            PermissionBehaviorDto::Allow,
            WritableScopeDto::Project,
        );
        assert_eq!(rule.value.tool_name, "Bash");
        assert_eq!(rule.value.rule_content.as_deref(), Some("ls:*"));
        assert_eq!(rule.behavior, permission::PermissionBehavior::Allow);
        assert_eq!(
            rule.source,
            permission::PermissionRuleSource::Settings(protocol::SettingsScope::Project)
        );
    }

    /// Correction #1: parsing is infallible. A malformed rule string (an
    /// unbalanced paren) must NOT be rejected — it degrades to a bare tool
    /// name carrying the whole input, matching claude-code's own parser. This
    /// pins that no validation was smuggled into the mapping layer.
    #[test]
    fn permission_rule_from_wire_never_rejects_malformed_input() {
        use client_protocol::commands::PermissionBehaviorDto;
        let rule = permission_rule_from_wire(
            "Bash(ls:*",
            PermissionBehaviorDto::Deny,
            WritableScopeDto::User,
        );
        assert_eq!(
            rule.value.tool_name, "Bash(ls:*",
            "an unbalanced-paren string must degrade to a bare tool name carrying \
             the whole input, not be rejected"
        );
        assert_eq!(rule.value.rule_content, None);
    }

    #[test]
    fn permission_paths_derives_cwd_from_project_dir() {
        let paths = SettingsPaths {
            lingxi_home: PathBuf::from("/home/user/.lingxi"),
            project_dir: PathBuf::from("/repo"),
        };
        let perm_paths = permission_paths(&paths);
        assert_eq!(perm_paths.lingxi_home, PathBuf::from("/home/user/.lingxi"));
        assert_eq!(
            perm_paths.cwd,
            PathBuf::from("/repo"),
            "PermissionPaths::cwd must be SettingsPaths::project_dir, not a second root"
        );
    }
}
