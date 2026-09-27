//! Persisted-schema compatibility coverage (spec §D).
//!
//! `tests/fixtures/v1/` holds a complete checked-in on-disk store —
//! `apps/index.json` plus every per-app document (`runtime.json`,
//! `permissions.json`, `workspace/.lingxi/app.json`,
//! `workspace/.lingxi/app.manifest.json`) — captured at `schemaVersion` 1.
//! The tree is produced by DRIVING A REAL [`AppService`] through a legal
//! trace (deterministic [`FixedClock`]), so every persisted shape is one a
//! legal writer actually produced.
//!
//! This crate now intentionally has NO v1→v2 migration for pre-release local
//! app stores. The checked-in v1 fixture therefore serves as a pinned negative
//! case: loads must fail closed with explicit "clear apps/ and recreate"
//! guidance, instead of silently laundering an old store into the new schema.
//!
//! Positive coverage comes from a fresh v3 store driven through the real
//! service path at test time. That store must load to the expected in-memory
//! states and re-persist byte-for-byte through the real writers.

use local_apps::storage::{self, save_app_files, save_index};
use local_apps::test_support::FixedClock;
use local_apps::{
    save_manifest, save_permissions, AppDependencyRecord, AppDependencySnapshot,
    AppDependencyState, AppErrorCode, AppEventObserver, AppLayout, AppManifest, AppPermissions,
    AppRecord, AppRuntimeMode, AppRuntimeProfile, AppRuntimeProfileBinding, AppRuntimeRecord,
    AppRuntimeState, AppService, AppState, AppSurface, AppTemplateOrigin, NoopAppEventObserver,
    APPS_SCHEMA_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Directory holding the checked-in v1 store fixture.
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1")
}

fn profiled_manifest_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2_profiled_manifest.json")
}

/// The fixture tests operate on the ONE checked-in fixture store, and each
/// scrubs the runtime lock artifact its load creates — an unlink racing the
/// sibling test's lock ACQUISITION can surface as a spurious open failure,
/// so the tests serialize on this guard (a tokio mutex: the async test holds
/// it across awaits, which a std guard must never do).
static FIXTURE_STORE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Best-effort removal of the runtime lock artifact `load_all` creates in
/// the CHECKED-IN fixture tree, so running the tests leaves the repo clean.
fn scrub_fixture_lock() {
    let _ = std::fs::remove_file(fixtures_root().join(storage::index_lock_rel()));
}

/// Base timestamp of the fixture trace (epoch ms).
const T0: u64 = 1_753_000_000_000;

/// Persist the post-scaffold v2 contract that a legal profile confirmation and
/// dependency transaction commit together. The catalog itself lives in the
/// host crate, so this core compatibility test pins a deterministic valid
/// binding while exercising the real manifest and dependency-record writers.
fn save_profiled_contract(root: &Path, app: &AppState) {
    let contract_sha256 = "a".repeat(64);
    let snapshot = AppDependencySnapshot {
        requested_sha256: "b".repeat(64),
        package_sha256: "c".repeat(64),
        lockfile_sha256: "d".repeat(64),
        dependency_tree_sha256: "e".repeat(64),
        sbom_sha256: "f".repeat(64),
        toolchain_key: "pnpm@11.22.0/node@24.18.1".to_string(),
        verified_profile_contract_sha256: contract_sha256.clone(),
    };
    let mut manifest = AppManifest::for_new_app(&app.record.id, &app.record.name);
    manifest.surface = Some(AppSurface::Dom);
    manifest.runtime_profile = Some(AppRuntimeProfileBinding {
        family: AppRuntimeProfile::ReactDom,
        revision: 1,
        contract_sha256,
    });
    manifest.dependency_snapshot = Some(snapshot.clone());
    manifest.template_origin = Some(AppTemplateOrigin {
        plugin_id: AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
        plugin_version: "builtin".into(),
        template_id: "react-dom-r1".into(),
        template_sha256: "a".repeat(64),
    });
    let layout = AppLayout::new(root, &app.record.id).expect("profiled layout");
    save_manifest(&layout, &manifest).expect("save profiled manifest");
    storage::save_dependency_record(
        root,
        &AppDependencyRecord {
            schema_version: APPS_SCHEMA_VERSION,
            app_id: app.record.id.clone(),
            state: AppDependencyState::Ready,
            lockfile_sha256: Some(snapshot.lockfile_sha256),
            toolchain_key: Some(snapshot.toolchain_key),
            install_attempts: 1,
            last_error: None,
            // The dependency transaction commits before later workflow/runtime
            // state changes, so its timestamp is independent of record.updatedAtMs.
            updated_at_ms: T0,
        },
    )
    .expect("save ready dependency record");
}

/// Seed one brand-new app with a PINNED id — byte-for-byte the persistence
/// `AppService::create_app_with_git` performs (per-app files first, index
/// entry last); only the random id mint is bypassed so the fixture paths
/// stay stable.
fn seed_app(
    root: &Path,
    existing: &mut Vec<AppState>,
    id: &str,
    name: &str,
    brief: &str,
    conversation_id: Option<String>,
    git_enabled: bool,
) {
    let app = AppState::create_with_git(
        id.to_string(),
        name.to_string(),
        brief.to_string(),
        conversation_id,
        git_enabled,
        T0,
    );
    save_app_files(root, &app).expect("seed app files");
    // `create_app_with_git` also mints the native contract and the
    // permission document; both are pinned like the other three. Each
    // writer creates the layout skeleton itself, so no separate
    // `initialize` is needed.
    let layout = AppLayout::new(root, id).expect("seed layout");
    save_profiled_contract(root, &app);
    save_permissions(&layout, &AppPermissions::default()).expect("seed permissions");
    existing.push(app);
    let records: Vec<AppRecord> = existing.iter().map(|app| app.record.clone()).collect();
    save_index(root, &records).expect("seed index");
}

/// Drive a REAL [`AppService`] through the legal trace that produces the
/// fixture store under `root`. Fully deterministic.
async fn drive_canonical_store(root: &Path) {
    let mut seeded = Vec::new();
    seed_app(
        root,
        &mut seeded,
        "aaaa1111",
        "Fixture Ready",
        "a ready fixture app with a full runtime record",
        Some("conv-fixture-1".to_string()),
        true,
    );
    seed_app(
        root,
        &mut seeded,
        "bbbb2222",
        "Fixture Minimal",
        "a minimal fixture app without git",
        None,
        false,
    );

    let clock = Arc::new(FixedClock::new(T0));
    let service_clock: Arc<FixedClock> = Arc::clone(&clock);
    let service = AppService::load(
        root,
        service_clock,
        Arc::new(NoopAppEventObserver) as Arc<dyn AppEventObserver>,
    )
    .await
    .expect("load seeded store");

    let mut now = T0;
    let mut advance_to = |target: u64| {
        clock.advance_ms(target - now);
        now = target;
    };

    // ── aaaa1111: the distribution picks a runtime mode…
    advance_to(T0 + 200);
    service
        .set_runtime_mode("aaaa1111", AppRuntimeMode::StaticExport)
        .await
        .expect("set runtime mode");
    // …and a start/fail cycle populates every runtime field.
    advance_to(T0 + 300);
    service
        .update_runtime_record(
            "aaaa1111",
            AppRuntimeState::Starting,
            Some(3111),
            Some(4242),
            None,
        )
        .await
        .expect("runtime starting");
    advance_to(T0 + 400);
    service
        .update_runtime_record(
            "aaaa1111",
            AppRuntimeState::Failed,
            None,
            Some(4242),
            Some("dev server exited with code 1".to_string()),
        )
        .await
        .expect("runtime failed");
    service.flush_events().await;
}

/// The in-memory states the fixture tree must load to, spelled out as
/// literals — nothing is spliced from the loaded store, the whole shape is
/// pinned independently of the loader.
fn expected_states() -> Vec<AppState> {
    let ready = AppState {
        record: AppRecord {
            id: "aaaa1111".to_string(),
            name: "Fixture Ready".to_string(),
            brief: "a ready fixture app with a full runtime record".to_string(),
            workflow_model: None,
            mcp_intent: None,
            git_enabled: true,
            scaffolded: true,
            created_at_ms: T0,
            updated_at_ms: T0,
            conversation_id: Some("conv-fixture-1".to_string()),
            origin_cwd: None,
            init_session_id: None,
            workspace_rel: "apps/aaaa1111/workspace".to_string(),
        },
        runtime: AppRuntimeRecord {
            schema_version: APPS_SCHEMA_VERSION,
            app_id: "aaaa1111".to_string(),
            state: AppRuntimeState::Failed,
            mode: Some(AppRuntimeMode::StaticExport),
            port: Some(3111),
            pid: Some(4242),
            last_error: Some("dev server exited with code 1".to_string()),
            updated_at_ms: T0 + 400,
        },
    };

    let minimal = AppState {
        record: AppRecord {
            id: "bbbb2222".to_string(),
            name: "Fixture Minimal".to_string(),
            brief: "a minimal fixture app without git".to_string(),
            workflow_model: None,
            mcp_intent: None,
            git_enabled: false,
            scaffolded: true,
            created_at_ms: T0,
            updated_at_ms: T0,
            conversation_id: None,
            origin_cwd: None,
            init_session_id: None,
            workspace_rel: "apps/bbbb2222/workspace".to_string(),
        },
        runtime: AppRuntimeRecord {
            schema_version: APPS_SCHEMA_VERSION,
            app_id: "bbbb2222".to_string(),
            state: AppRuntimeState::Stopped,
            mode: None,
            port: None,
            pid: None,
            last_error: None,
            updated_at_ms: T0,
        },
    };

    vec![ready, minimal]
}

/// Persist `states` under `root` through the REAL writer path (per-app files
/// first, index last — the same order the service commits in). The manifest
/// and permission documents are not part of [`AppState`], so each is minted
/// from the same PRODUCER `create_app_with_git` uses ([`seed_app`] does the
/// same).
fn write_current_store(root: &Path, states: &[AppState]) {
    for app in states {
        save_app_files(root, app).expect("save app files");
        let layout = AppLayout::new(root, app.record.id.clone()).expect("target layout");
        save_profiled_contract(root, app);
        save_permissions(&layout, &AppPermissions::default()).expect("save permissions");
    }
    let records: Vec<AppRecord> = states.iter().map(|app| app.record.clone()).collect();
    save_index(root, &records).expect("save index");
}

/// Every file under `root`, as sorted root-relative paths. The advisory
/// index lock (`apps/index.lock`) is excluded: it is a runtime artifact
/// created by merely LOADING a store, not part of the persisted document
/// contract these goldens pin.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .expect("walked path is under root")
                        .to_path_buf(),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.retain(|rel| *rel != storage::index_lock_rel());
    out.sort();
    out
}

#[test]
fn schema_v2_local_app_fixture_is_rejected() {
    let bytes =
        std::fs::read(profiled_manifest_fixture()).expect("read v2 profiled manifest fixture");
    let manifest: AppManifest =
        serde_json::from_slice(&bytes).expect("fixture must deserialize as schema v2");
    assert_eq!(manifest.app_id, "profiled-v2");
    assert_eq!(manifest.surface, Some(local_apps::AppSurface::Canvas));
    let runtime_profile = manifest.runtime_profile.as_ref().expect("runtime profile");
    assert_eq!(
        runtime_profile.family,
        local_apps::AppRuntimeProfile::Three3d
    );
    assert_eq!(
        runtime_profile.contract_sha256,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    let snapshot = manifest
        .dependency_snapshot
        .as_ref()
        .expect("dependency snapshot");
    assert_eq!(
        snapshot.verified_profile_contract_sha256,
        runtime_profile.contract_sha256
    );
    assert_eq!(snapshot.toolchain_key, "pnpm@11.22.0/node@24.18.1");
    assert_eq!(
        {
            let mut serialized = serde_json::to_vec_pretty(&manifest).expect("serialize manifest");
            serialized.push(b'\n');
            serialized
        },
        bytes,
        "the checked-in v2 fixture must stay byte-for-byte stable"
    );
    assert!(manifest.runtime_contract_hash().is_err());
    assert!(manifest.dependency_snapshot_hash().is_err());
}

/// Negative direction: the checked-in v1 tree must fail closed with reset
/// guidance rather than being auto-migrated.
#[test]
fn fixture_v1_store_is_rejected_with_reset_guidance() {
    let _store = FIXTURE_STORE.blocking_lock();
    let error = storage::load_all(&fixtures_root())
        .expect_err("the checked-in v1 fixture store must stay unsupported without a migration");
    scrub_fixture_lock();
    assert_eq!(error.code(), AppErrorCode::StorageCorrupt);
    let message = error.to_string();
    assert!(message.contains("unsupported schemaVersion 1"), "{message}");
    assert!(message.contains("delete apps/"), "{message}");
    assert!(message.contains("清除应用开发数据"), "{message}");
}

/// Positive direction: a fresh v3 store must round-trip through the real load
/// and write paths without byte drift.
#[tokio::test]
async fn fresh_v3_store_round_trips_through_real_writers() {
    let fixtures = tempfile::tempdir().expect("tempdir");
    drive_canonical_store(fixtures.path()).await;

    let states = storage::load_all(fixtures.path()).expect("fresh v3 store loads");
    assert_eq!(states, expected_states());
    for app in &states {
        let layout = AppLayout::new(fixtures.path(), &app.record.id).expect("profiled layout");
        let manifest = local_apps::load_manifest(&layout).expect("load profiled manifest");
        assert_eq!(manifest.surface, Some(AppSurface::Dom));
        assert_eq!(
            manifest
                .runtime_profile
                .as_ref()
                .map(|binding| binding.family),
            Some(AppRuntimeProfile::ReactDom)
        );
        assert!(manifest.dependency_snapshot.is_some());
        assert_eq!(
            storage::load_dependency_record(fixtures.path(), &app.record)
                .expect("load dependency record")
                .state,
            AppDependencyState::Ready
        );
    }

    let rewritten = tempfile::tempdir().expect("tempdir");
    write_current_store(rewritten.path(), &states);

    let written_files = walk_files(rewritten.path());
    let fixture_files = walk_files(fixtures.path());
    assert_eq!(
        written_files, fixture_files,
        "the writers and the freshly produced v2 store must contain the same files"
    );

    let mut failures = Vec::new();
    for rel in &fixture_files {
        let want = std::fs::read_to_string(fixtures.path().join(rel))
            .unwrap_or_else(|error| panic!("read fixture {}: {error}", rel.display()));
        let got = std::fs::read_to_string(rewritten.path().join(rel))
            .unwrap_or_else(|error| panic!("read written {}: {error}", rel.display()));
        if got != want {
            failures.push(format!(
                "`{}` drifted from the persisted v2 contract.\n--- fixture ---\n{want}\
                 --- writer ---\n{got}",
                rel.display()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} persisted document(s) drifted:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A pipeline-era v1 store is also unsupported without an explicit migration;
/// the failure should point developers at resetting their local app data.
#[test]
fn a_legacy_pipeline_store_is_rejected_with_reset_guidance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let app_dir = root.join("apps/legacy01");
    std::fs::create_dir_all(app_dir.join("workspace/.lingxi")).expect("mkdir");

    // A legacy record mid-pipeline, exactly as a pre-v3 build persisted it.
    // This fixture is about the WORKFLOW-STATE migration (mid-pipeline
    // strings collapsing to `draft`), which is orthogonal to `scaffolded`'s
    // own no-default invariant (pinned separately below) — so it carries a
    // `scaffolded` value like any other current-shape record would.
    let legacy_record = r#"{
    "id": "legacy01",
    "name": "Legacy Habits",
    "brief": "a legacy pipeline app",
    "gitEnabled": true,
    "scaffolded": true,
    "createdAtMs": 1753000000000,
    "updatedAtMs": 1753000000700,
    "workflowState": "awaiting_preview_confirmation",
    "conversationId": "conv-legacy-1",
    "workspaceRel": "apps/legacy01/workspace"
  }"#;
    std::fs::write(
        root.join("apps/index.json"),
        format!(
            "{{\n  \"schemaVersion\": 1,\n  \"apps\": [\n    {}\n  ]\n}}\n",
            legacy_record.trim()
        ),
    )
    .expect("write index");
    std::fs::write(
        app_dir.join("workspace/.lingxi/app.json"),
        format!(
            "{{\n  \"schemaVersion\": 1,\n  \"app\": {}\n}}\n",
            legacy_record.trim()
        ),
    )
    .expect("write mirror");
    std::fs::write(
        app_dir.join("runtime.json"),
        "{\n  \"schemaVersion\": 1,\n  \"appId\": \"legacy01\",\n  \"state\": \"stopped\",\n  \"updatedAtMs\": 1753000000000\n}\n",
    )
    .expect("write runtime");

    // Plausible legacy pipeline documents (shapes copied from the pre-v3
    // fixture store): a pending preview gate + queued continuations, and a
    // design draft with answers and a pending suggestion.
    let stale_interactions = r#"{
  "schemaVersion": 1,
  "pending": {
    "interactionId": "int-7df09b4cb739",
    "appId": "legacy01",
    "kind": "preview",
    "revision": 1,
    "createdAtMs": 1753000000700
  },
  "nextSeq": 5,
  "lastDeliveredSeq": 1,
  "undelivered": [
    {
      "seq": 3,
      "appId": "legacy01",
      "kind": "design_confirmed",
      "payload": {
        "revision": 1
      },
      "createdAtMs": 1753000000400
    },
    {
      "seq": 4,
      "appId": "legacy01",
      "kind": "revision_requested",
      "payload": {
        "prompt": "make the header darker"
      },
      "createdAtMs": 1753000000550
    }
  ]
}
"#;
    let stale_design_spec = r##"{
  "schemaVersion": 1,
  "revision": 1,
  "questionnaire": [],
  "fields": {
    "accent": {
      "kind": "color",
      "value": "#3366ff"
    },
    "title": {
      "kind": "short_text",
      "value": "Habit Tracker"
    }
  },
  "pendingSuggestion": {
    "suggestionId": "sugg-a87290bde485",
    "patch": {
      "ops": [
        {
          "op": "set",
          "fieldId": "accent",
          "value": {
            "kind": "color",
            "value": "#112233"
          }
        }
      ],
      "note": "tone down the accent"
    },
    "basedOnRevision": 1
  },
  "confirmedRevision": 1
}
"##;
    let interactions_path = app_dir.join("interactions.json");
    let design_spec_path = app_dir.join("workspace/.lingxi/design-spec.json");
    std::fs::write(&interactions_path, stale_interactions).expect("write interactions");
    std::fs::write(&design_spec_path, stale_design_spec).expect("write design spec");

    let error =
        storage::load_all(root).expect_err("a legacy v1 pipeline store must stay unsupported");
    assert_eq!(error.code(), AppErrorCode::StorageCorrupt);
    let message = error.to_string();
    assert!(message.contains("unsupported schemaVersion 1"), "{message}");
    assert!(message.contains("delete apps/"), "{message}");
    assert!(message.contains("清除应用开发数据"), "{message}");

    // The stale pipeline documents are not touched by the failed load.
    assert_eq!(
        std::fs::read_to_string(&interactions_path).unwrap(),
        stale_interactions
    );
    assert_eq!(
        std::fs::read_to_string(&design_spec_path).unwrap(),
        stale_design_spec
    );
}

#[test]
fn a_record_without_scaffolded_fails_to_load_instead_of_defaulting_to_a_shell() {
    // §A.1 clean-install: a missing field is an unsupported old store, NOT a
    // shell. Silently defaulting to `false` would let the next LocalAppScaffold
    // WIPE a real app's source (§C.0.1 clears the editable surface).
    //
    // Every OTHER required field is present under its real wire spelling
    // (`AppRecord` is `#[serde(rename_all = "camelCase")]`) so `scaffolded`
    // is the only thing that can be missing — otherwise serde's derived
    // error names whichever required field it hits first, which would not
    // necessarily be `scaffolded`.
    let json = r#"{"id":"legacy","name":"Legacy","brief":"b","gitEnabled":true,
        "createdAtMs":1,"updatedAtMs":1,"workflowState":"draft",
        "workspaceRel":"apps/legacy/workspace"}"#;
    let err = serde_json::from_str::<AppRecord>(json)
        .expect_err("a record without `scaffolded` must not load");
    assert!(
        err.to_string().contains("scaffolded"),
        "the error must name the missing field; got {err}"
    );
}
