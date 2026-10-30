use super::*;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path};
fn authoring(source_directory: &str) -> AuthoringRef {
    AuthoringRef {
        source_directory: source_directory.into(),
        workspace_id: Some(uuid::Uuid::new_v4().to_string()),
        project_ref: Some(ProjectRef::MobileProject {
            project_id: uuid::Uuid::new_v4().to_string(),
        }),
        authoring_session_id: Some(uuid::Uuid::new_v4().to_string()),
        app_id: None,
    }
}
fn manifest() -> Value {
    json!({"schema":2,"name":"Test app","entry":{"type":"static","path":"index.html"},"targets":[{"platform":"ios","environment":"web"},{"platform":"android","environment":"web"},{"platform":"macos","environment":"web"}],"package":{"files":["index.html"],"dependencyLocks":[]}})
}
fn source(path: &Path, value: &Value) {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join("lingxi-app.json"), value.to_string()).unwrap();
    fs::write(path.join("index.html"), "<!doctype html><html>one</html>").unwrap();
}
fn setup() -> (tempfile::TempDir, AppLibrary, PublishRequest) {
    let dir = tempfile::tempdir().unwrap();
    let lib = AppLibrary::open(dir.path().join("config")).unwrap();
    let src = dir.path().join("project");
    source(&src, &manifest());
    let request = PublishRequest {
        app_id: None,
        expected_revision: 0,
        source_path: src.to_string_lossy().into(),
        authoring: Some(authoring("/root/.lingxi/.apps/test")),
        market_origin: None,
    };
    (dir, lib, request)
}

#[test]
fn exclusive_runtime_owner_recovers_only_after_previous_owner_releases() {
    let (_root, library, request) = setup();
    let owner = library.acquire_runtime_lease().unwrap();
    let published = library.publish(request).unwrap();
    let mut run = library
        .prepare_run(&published.record.app_id, 1)
        .unwrap()
        .run;
    run.phase = RunPhase::Running;
    library.update_run(run.clone()).unwrap();
    let reader = AppLibrary::open(_root.path().join("config")).unwrap();
    assert!(matches!(
        reader.acquire_runtime_lease(),
        Err(AppError::Busy(_))
    ));
    assert_eq!(
        reader.runs(&run.app_id).unwrap()[0].phase,
        RunPhase::Running
    );
    drop(owner);
    let recovered = reader.acquire_runtime_lease().unwrap();
    assert_eq!(recovered.recovered_runs, 1);
    assert_eq!(
        reader.runs(&run.app_id).unwrap()[0].phase,
        RunPhase::Interrupted
    );
}
#[test]
fn strict_manifest_requires_schema_targets_known_capabilities() {
    for mutate in [0, 1, 2, 3, 4] {
        let mut m = manifest();
        match mutate {
            0 => m["schema"] = json!(1),
            1 => m["targets"] = json!([]),
            2 => m["capabilities"] = json!(["unrestricted-root"]),
            3 => m["unknownField"] = json!(true),
            _ => m["entry"]["path"] = json!("../escape"),
        };
        assert!(Manifest::parse(&m.to_string()).is_err(), "{m}");
    }
}
#[test]
fn agent_requirements_are_not_runtime_grants() {
    let mut m = manifest();
    m["capabilities"] = json!(["agent"]);
    m["agent"] = json!({"tools":["Read","mcp__calendar"]});
    assert!(Manifest::parse(&m.to_string()).is_ok());
    m["agent"]["model"] = json!("gpt");
    assert!(Manifest::parse(&m.to_string()).is_err());
}
#[test]
fn target_matching_checks_version_abi_architecture_channel() {
    let mut m = manifest();
    m["targets"] = json!([{"platform":"ios","environment":"mobile-linux","architecture":"aarch64","runtimeVersion":"1.2.0","runtimeAbi":"ish-arm64-v1","channels":["development"],"capabilities":["process"]}]);
    m["capabilities"] = json!(["process"]);
    let m = Manifest::parse(&m.to_string()).unwrap();
    let mut host:Target=serde_json::from_value(json!({"platform":"ios","environment":"mobile-linux","architecture":"aarch64","runtimeVersion":"1.3.0","runtimeAbi":"ish-arm64-v1","channels":["development"],"capabilities":["process"]})).unwrap();
    assert!(m.compatible_with(&host).unwrap());
    host.runtime_version = Some("1.1.0".into());
    assert!(!m.compatible_with(&host).unwrap());
    host.runtime_version = Some("2.0.0".into());
    host.runtime_abi = Some("different".into());
    assert!(!m.compatible_with(&host).unwrap());
    host.runtime_abi = Some("ish-arm64-v1".into());
    host.channels = vec!["app-store".into()];
    assert!(!m.compatible_with(&host).unwrap());
}
#[test]
fn rejects_dependency_cycles_and_unknown_process() {
    let mut m = manifest();
    m["processes"] = json!([{"name":"a","command":"a","dependsOn":["b"]},{"name":"b","command":"b","dependsOn":["a"]}]);
    assert!(Manifest::parse(&m.to_string()).is_err());
    m["processes"] = json!([]);
    m["entry"] = json!({"type":"server","process":"missing"});
    assert!(Manifest::parse(&m.to_string()).is_err());
}

#[test]
fn execution_steps_require_host_process_support_even_without_explicit_capability() {
    let mut value = manifest();
    value["install"] = json!([{"command":"prepare application"}]);
    let release = Manifest::parse(&value.to_string()).unwrap();
    let mut host: Target = serde_json::from_value(
        json!({"platform":"ios","environment":"web","channels":["store"],"capabilities":[]}),
    )
    .unwrap();
    assert!(!release.compatible_with(&host).unwrap());
    host.capabilities.push("process".into());
    assert!(release.compatible_with(&host).unwrap());
}
#[test]
fn publication_snapshot_outlives_source_and_session() {
    let (_dir, lib, req) = setup();
    let result = lib.publish(req.clone()).unwrap();
    fs::remove_dir_all(req.source_path).unwrap();
    assert_eq!(lib.list().unwrap().len(), 1);
    assert!(lib.verify_release(&result.record.app_id, 1).is_ok());
    let resolution = lib
        .prepare_authoring(&result.record.app_id, &BTreeMap::new(), false)
        .unwrap();
    assert_eq!(resolution.action, "sourceMissing");
}
#[test]
fn staged_update_and_failed_publish_leave_active_revision_and_data() {
    let (_dir, lib, mut req) = setup();
    let first = lib.publish(req.clone()).unwrap();
    let app = &first.record.app_id;
    fs::write(lib.data_path(app).unwrap().join("user.txt"), "private").unwrap();
    req.app_id = Some(app.clone());
    req.expected_revision = 1;
    fs::write(
        Path::new(&req.source_path).join("index.html"),
        "<!doctype html>two",
    )
    .unwrap();
    let second = lib.publish(req.clone()).unwrap();
    assert_eq!(second.record.latest_revision, 2);
    assert_eq!(second.record.current_revision, 1);
    assert_ne!(second.release.digest, first.release.digest);
    req.expected_revision = 2;
    fs::remove_file(Path::new(&req.source_path).join("index.html")).unwrap();
    assert!(lib.publish(req).is_err());
    assert_eq!(lib.get(app).unwrap().latest_revision, 2);
    assert_eq!(
        fs::read_to_string(lib.data_path(app).unwrap().join("user.txt")).unwrap(),
        "private"
    );
    assert!(lib.activate_release(app, 2).is_ok());
}
#[test]
fn competing_publishers_cannot_silently_overwrite() {
    let (_dir, lib, mut req) = setup();
    let first = lib.publish(req.clone()).unwrap();
    req.app_id = Some(first.record.app_id);
    req.expected_revision = 1;
    let other = AppLibrary::open(lib.root().parent().unwrap().parent().unwrap()).unwrap();
    let clone = req.clone();
    let one = std::thread::spawn(move || other.publish(clone));
    let two = lib.publish(req);
    let one = one.join().unwrap();
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    assert!(matches!(
        one.err().or(two.err()),
        Some(AppError::Conflict {
            expected: 1,
            actual: 2
        })
    ));
}
#[test]
fn execution_consent_applies_before_any_preparation_and_exact_content() {
    let (_dir, lib, mut req) = setup();
    let mut m = manifest();
    m["install"] = json!([{"command":"npm ci"}]);
    source(Path::new(&req.source_path), &m);
    let first = lib.publish(req.clone()).unwrap();
    let app = &first.record.app_id;
    assert!(matches!(
        lib.prepare_run(app, 1),
        Err(AppError::ConsentRequired(_))
    ));
    assert!(lib.runs(app).unwrap().is_empty());
    lib.grant_execution(app, &first.release.digest).unwrap();
    let prep = lib.prepare_run(app, 1).unwrap();
    assert!(Path::new(&prep.work_path).join("index.html").exists());
    req.app_id = Some(app.clone());
    req.expected_revision = 1;
    fs::write(Path::new(&req.source_path).join("index.html"), "changed").unwrap();
    lib.publish(req).unwrap();
    assert!(matches!(
        lib.prepare_run(app, 2),
        Err(AppError::ConsentRequired(_))
    ));
    lib.revoke_execution(app).unwrap();
    assert!(matches!(
        lib.require_execution_grant(app, 1),
        Err(AppError::ConsentRequired(_))
    ));
}
#[test]
fn release_tampering_is_rejected_before_run() {
    let (_dir, lib, req) = setup();
    let first = lib.publish(req).unwrap();
    fs::write(
        lib.source_path(&first.record.app_id, 1)
            .unwrap()
            .join("index.html"),
        "tampered",
    )
    .unwrap();
    assert!(lib.prepare_run(&first.record.app_id, 1).is_err());
}
#[test]
fn separate_runs_and_data_do_not_modify_release() {
    let (_dir, lib, req) = setup();
    let first = lib.publish(req).unwrap();
    let a = lib.prepare_run(&first.record.app_id, 1).unwrap();
    let b = lib.prepare_run(&first.record.app_id, 1).unwrap();
    assert_ne!(a.work_path, b.work_path);
    assert_eq!(a.data_path, b.data_path);
    fs::write(Path::new(&a.work_path).join("index.html"), "mutable").unwrap();
    assert!(lib.verify_release(&first.record.app_id, 1).is_ok());
    let mut stopped = a.run.clone();
    stopped.phase = RunPhase::Stopped;
    lib.update_run(stopped.clone()).unwrap();
    stopped.phase = RunPhase::Running;
    assert!(lib.update_run(stopped).is_err());
    assert!(lib.remove(&first.record.app_id).is_err());
}
#[test]
fn schema_changes_need_an_explicit_migration_transaction_before_preparation_or_activation() {
    let (_dir, lib, mut req) = setup();
    let first = lib.publish(req.clone()).unwrap();
    let data = lib.data_path(&first.record.app_id).unwrap();
    fs::write(data.join("database"), "schema1-data").unwrap();
    let mut m = manifest();
    m["dataSchema"] = json!({"version":2,"compatibleFrom":1});
    source(Path::new(&req.source_path), &m);
    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    lib.publish(req).unwrap();
    for error in [
        lib.activate_release(&first.record.app_id, 2).unwrap_err(),
        lib.prepare_run(&first.record.app_id, 2).unwrap_err(),
    ] {
        assert!(matches!(
            error,
            AppError::DataMigrationRequired {
                current: 1,
                target: 2
            }
        ));
        assert_eq!(error.code(), "dataMigrationRequired");
    }
    assert!(lib.runs(&first.record.app_id).unwrap().is_empty());
    assert_eq!(lib.get(&first.record.app_id).unwrap().current_revision, 1);
    assert_eq!(
        fs::read_to_string(data.join("database")).unwrap(),
        "schema1-data"
    );
}
#[test]
fn schema_downgrade_is_blocked_and_initial_schema_can_be_newer() {
    let (_dir, lib, mut req) = setup();
    let mut m = manifest();
    m["dataSchema"] = json!({"version":2,"compatibleFrom":1});
    source(Path::new(&req.source_path), &m);
    let first = lib.publish(req.clone()).unwrap();
    assert!(lib.prepare_run(&first.record.app_id, 1).is_ok());
    source(Path::new(&req.source_path), &manifest());
    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    lib.publish(req).unwrap();
    assert!(matches!(
        lib.activate_release(&first.record.app_id, 2),
        Err(AppError::DataMigrationRequired {
            current: 2,
            target: 1
        })
    ));
}
#[test]
fn continued_development_only_reuses_recorded_matching_session() {
    let (_dir, lib, req) = setup();
    let first = lib.publish(req).unwrap();
    let binding = first.record.authoring.clone().unwrap();
    let session_id = binding.authoring_session_id.clone().unwrap();
    let context = AuthoringSessionContext {
        workspace_id: binding.workspace_id.clone().unwrap(),
        source_directory: binding.source_directory.clone(),
        project_ref: binding.project_ref.clone().unwrap(),
        mode: "code".into(),
    };
    let mut dirs = BTreeMap::new();
    dirs.insert("other-active-session".into(), context.clone());
    dirs.insert(
        session_id.clone(),
        AuthoringSessionContext {
            source_directory: "/root/elsewhere".into(),
            ..context.clone()
        },
    );
    assert_eq!(
        lib.prepare_authoring(&first.record.app_id, &dirs, true)
            .unwrap()
            .action,
        "createSession"
    );
    dirs.insert(
        session_id.clone(),
        AuthoringSessionContext {
            mode: "chat".into(),
            ..context.clone()
        },
    );
    assert_eq!(
        lib.prepare_authoring(&first.record.app_id, &dirs, true)
            .unwrap()
            .action,
        "createSession"
    );
    dirs.insert(session_id.clone(), context);
    let prepared = lib
        .prepare_authoring(&first.record.app_id, &dirs, true)
        .unwrap();
    assert_eq!(prepared.action, "resumeSession");
    assert_eq!(
        prepared.authoring_session_id.as_deref(),
        Some(session_id.as_str())
    );
}

#[test]
fn authoring_rejects_obsolete_fields_instead_of_rebinding_them() {
    let (_dir, lib, request) = setup();
    for field in ["sourceSessionId", "projectId"] {
        let mut value = serde_json::to_value(&request).unwrap();
        value["op"] = json!("publish");
        value["authoring"] = serde_json::to_value(authoring("/root/.lingxi/.apps/test")).unwrap();
        value["authoring"][field] = json!("old-identity");
        assert!(lib.request_json(&value.to_string()).is_err(), "{field}");
        assert!(lib.list().unwrap().is_empty());
    }
}

#[test]
fn authoring_binding_requires_owner_project_and_real_session_identity() {
    let (_dir, lib, req) = setup();
    let first = lib.publish(req).unwrap();
    let mut wrong_owner = authoring("/root/.lingxi/.apps/other");
    wrong_owner.app_id = Some("a00000000000000000000000000000000".into());
    assert!(lib
        .bind_authoring(&first.record.app_id, wrong_owner)
        .is_err());

    let mut invalid_project = authoring("/root/.lingxi/.apps/other");
    invalid_project.project_ref = Some(ProjectRef::MobileProject {
        project_id: "not-a-uuid".into(),
    });
    assert!(lib
        .bind_authoring(&first.record.app_id, invalid_project)
        .is_err());

    let mut invalid_session = authoring("/root/.lingxi/.apps/other");
    invalid_session.authoring_session_id = Some("not-a-session-uuid".into());
    assert!(lib
        .bind_authoring(&first.record.app_id, invalid_session)
        .is_err());
}

#[test]
fn pending_authoring_requires_a_complete_ownerless_canonical_binding() {
    let (_dir, lib, _req) = setup();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let mut incomplete = authoring(source);
    incomplete.authoring_session_id = None;
    assert!(lib.register_pending_authoring(incomplete).is_err());

    let mut already_owned = authoring(source);
    already_owned.app_id = Some("a00000000000000000000000000000000".into());
    assert!(lib.register_pending_authoring(already_owned).is_err());

    let mut noncanonical = authoring("/home/user/.lingxi/.apps/../outside");
    assert!(lib
        .register_pending_authoring(noncanonical.clone())
        .is_err());
    noncanonical.source_directory = source.into();
    let saved = lib
        .register_pending_authoring(noncanonical.clone())
        .unwrap();
    assert_eq!(saved, noncanonical);
}

#[test]
fn registered_binding_is_consumed_by_first_publish_and_modify_resumes_same_code_session() {
    let (_dir, lib, mut req) = setup();
    let source_directory = "/home/user/.lingxi/.apps/notes-1234abcd";
    let binding = authoring(source_directory);
    let session_id = binding.authoring_session_id.clone().unwrap();
    lib.register_pending_authoring(binding.clone()).unwrap();
    req.authoring = None;

    let first = lib
        .publish_with_pending_authoring(req.clone(), source_directory)
        .unwrap();
    let saved = first.record.authoring.as_ref().unwrap();
    assert_eq!(saved.source_directory, source_directory);
    assert_eq!(saved.app_id.as_deref(), Some(first.record.app_id.as_str()));
    assert_eq!(saved.workspace_id, binding.workspace_id);
    assert_eq!(saved.project_ref, binding.project_ref);
    assert_eq!(
        saved.authoring_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        fs::read_dir(lib.root().join("pending-authoring"))
            .unwrap()
            .count(),
        0
    );

    let context = AuthoringSessionContext {
        workspace_id: binding.workspace_id.clone().unwrap(),
        source_directory: source_directory.into(),
        project_ref: binding.project_ref.clone().unwrap(),
        mode: "code".into(),
    };
    let mut sessions = BTreeMap::new();
    sessions.insert(session_id.clone(), context);
    let prepared = lib
        .prepare_authoring(&first.record.app_id, &sessions, true)
        .unwrap();
    assert_eq!(prepared.action, "resumeSession");
    assert_eq!(
        prepared.authoring_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert!(lib.register_pending_authoring(binding).is_err());

    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    let modified = lib.publish(req).unwrap();
    assert_eq!(modified.record.authoring, first.record.authoring);
}

#[test]
fn failed_first_publish_keeps_registered_authoring_for_retry() {
    let (_dir, lib, mut req) = setup();
    let source_directory = "/home/user/.lingxi/.apps/retry-1234abcd";
    lib.register_pending_authoring(authoring(source_directory))
        .unwrap();
    req.authoring = None;
    let manifest_path = Path::new(&req.source_path).join("lingxi-app.json");
    let valid_manifest = fs::read(&manifest_path).unwrap();
    fs::write(&manifest_path, "{\"schema\":99}").unwrap();
    assert!(lib
        .publish_with_pending_authoring(req.clone(), source_directory)
        .is_err());

    fs::write(&manifest_path, valid_manifest).unwrap();
    let retried = lib
        .publish_with_pending_authoring(req, source_directory)
        .unwrap();
    assert_eq!(
        retried.record.authoring.unwrap().source_directory,
        source_directory
    );
}

#[test]
fn project_refs_use_platform_tagged_wire_values() {
    let project_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        serde_json::to_value(ProjectRef::MobileProject {
            project_id: project_id.clone()
        })
        .unwrap(),
        json!({"kind":"mobileProject","projectId":project_id})
    );
    assert_eq!(
        serde_json::to_value(ProjectRef::DesktopPath {
            path: "/workspace/app".into()
        })
        .unwrap(),
        json!({"kind":"desktopPath","path":"/workspace/app"})
    );
}

#[test]
fn desktop_project_identity_can_contain_a_distinct_authoring_source_root() {
    let (_dir, lib, req) = setup();
    let published = lib.publish(req).unwrap();
    let mut binding = authoring("/workspace/project/local-app");
    binding.project_ref = Some(ProjectRef::DesktopPath {
        path: "/workspace/project".into(),
    });
    let updated = lib
        .bind_authoring(&published.record.app_id, binding)
        .unwrap();
    let bound = updated.authoring.unwrap();
    assert_eq!(bound.source_directory, "/workspace/project/local-app");
    assert_eq!(
        bound.project_ref,
        Some(ProjectRef::DesktopPath {
            path: "/workspace/project".into()
        })
    );
}
#[test]
fn public_export_is_only_explicit_sources_and_keeps_digest() {
    let (dir, lib, req) = setup();
    fs::write(Path::new(&req.source_path).join(".env"), "secret").unwrap();
    let first = lib.publish(req).unwrap();
    let dest = dir.path().join("export");
    lib.export(&first.record.app_id, 1, &dest).unwrap();
    assert!(!dest.join(".env").exists());
    assert!(!dest.join("record.json").exists());
    let imported = lib
        .publish(PublishRequest {
            app_id: None,
            expected_revision: 0,
            source_path: dest.to_string_lossy().into(),
            authoring: None,
            market_origin: None,
        })
        .unwrap();
    assert_ne!(first.record.app_id, imported.record.app_id);
    assert_eq!(first.release.digest, imported.release.digest);
    assert!(imported.record.execution_grants.is_empty());
}
#[test]
fn public_export_without_allowlist_and_secret_allowlist_fail() {
    let (dir, lib, req) = setup();
    let mut m = manifest();
    m.as_object_mut().unwrap().remove("package");
    source(Path::new(&req.source_path), &m);
    let first = lib.publish(req.clone()).unwrap();
    assert!(lib
        .export(&first.record.app_id, 1, &dir.path().join("export"))
        .is_err());
    m["package"] = json!({"files":["index.html",".env"]});
    source(Path::new(&req.source_path), &m);
    fs::write(Path::new(&req.source_path).join(".env"), "secret").unwrap();
    assert!(lib.publish(req).is_err());
}
#[cfg(unix)]
#[test]
fn source_symlink_and_traversal_rejected() {
    let (dir, lib, req) = setup();
    let secret = dir.path().join("secret");
    fs::write(&secret, "secret").unwrap();
    fs::remove_file(Path::new(&req.source_path).join("index.html")).unwrap();
    std::os::unix::fs::symlink(secret, Path::new(&req.source_path).join("index.html")).unwrap();
    assert!(lib.publish(req).is_err());
    for bad in ["../x", "/x", "a/../x", "a\\x", "a//x", "a/./x"] {
        assert!(relative_path(bad).is_err());
    }
}
#[test]
fn tombstone_preserves_data_and_releases() {
    let (_dir, lib, req) = setup();
    let first = lib.publish(req).unwrap();
    let data = lib.data_path(&first.record.app_id).unwrap();
    fs::write(data.join("private"), "keep").unwrap();
    let source = lib.source_path(&first.record.app_id, 1).unwrap();
    assert!(lib.remove(&first.record.app_id).unwrap().removed);
    assert!(lib.list().unwrap().is_empty());
    assert!(source.join("index.html").exists());
    assert_eq!(fs::read_to_string(data.join("private")).unwrap(), "keep");
}
#[test]
fn json_contract_and_unknown_operations_fail_closed() {
    let (_dir, lib, req) = setup();
    let mut value = serde_json::to_value(req).unwrap();
    value["op"] = json!("publish");
    let result = lib.request_json(&value.to_string()).unwrap();
    assert!(result["record"]["appId"].is_string());
    assert!(result["release"]["digest"].is_string());
    assert!(lib.request_json(r#"{"op":"start","appId":"app"}"#).is_err());
    assert!(lib
        .request_json(r#"{"op":"list","ignored":"field"}"#)
        .is_err());
}

#[test]
fn removed_archive_operations_cannot_convert_or_change_existing_files() {
    let (dir, lib, _) = setup();
    let archive = dir.path().join("config/local-apps/archive");
    fs::create_dir_all(&archive).unwrap();
    let old_index = dir.path().join("config/local-apps/index.json");
    fs::write(&old_index, b"unsupported old index").unwrap();
    fs::write(archive.join("private.db"), b"preserved data").unwrap();
    let destination = dir.path().join("converted-source");
    for request in [
        json!({"op":"legacyEntries"}),
        json!({"op":"legacyArchives"}),
        json!({"op":"importLegacy","legacyId":"archive","sourcePath":destination}),
    ] {
        assert!(lib.request_json(&request.to_string()).is_err());
        assert!(!destination.exists());
        assert!(lib.list().unwrap().is_empty());
        assert_eq!(fs::read(&old_index).unwrap(), b"unsupported old index");
        assert_eq!(
            fs::read(archive.join("private.db")).unwrap(),
            b"preserved data"
        );
    }
}

#[test]
fn current_source_import_dispatch_keeps_schema_two_and_independent_identity() {
    let (_dir, lib, request) = setup();
    let imported = lib
        .request_json(&json!({"op":"import","sourcePath":request.source_path}).to_string())
        .unwrap();
    let app_id = imported["record"]["appId"].as_str().unwrap();
    assert_eq!(imported["release"]["manifest"]["schema"], 2);
    assert_eq!(imported["record"]["latestRevision"], 1);
    assert!(imported["record"]["executionGrants"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(lib.list().unwrap().len(), 1);
    assert_eq!(lib.get(app_id).unwrap().app_id, app_id);
}
#[test]
fn restoring_source_creates_a_new_directory_and_rebinds_only_explicitly() {
    let (dir, lib, req) = setup();
    let published = lib.publish(req.clone()).unwrap();
    let authoring = authoring("/root/.lingxi/.apps/restored");
    let restored = lib
        .restore_authoring(
            &published.record.app_id,
            1,
            &dir.path().join("restored"),
            authoring.clone(),
        )
        .unwrap();
    let stored = restored.authoring.unwrap();
    assert_eq!(stored.source_directory, authoring.source_directory);
    assert_eq!(stored.workspace_id, authoring.workspace_id);
    assert_eq!(stored.project_ref, authoring.project_ref);
    assert_eq!(stored.authoring_session_id, authoring.authoring_session_id);
    assert_eq!(stored.app_id.as_deref(), Some(restored.app_id.as_str()));
    assert_eq!(restored.latest_revision, 1);
    assert!(lib
        .restore_authoring(
            &published.record.app_id,
            1,
            Path::new(&req.source_path),
            stored
        )
        .is_err());
}
#[test]
fn remix_requires_a_license_and_creates_an_independent_identity() {
    let (dir, lib, mut req) = setup();
    let first = lib.publish(req.clone()).unwrap();
    let authoring = authoring("/root/.lingxi/.apps/remix");
    assert!(lib
        .remix(
            &first.record.app_id,
            1,
            &dir.path().join("not-allowed"),
            authoring.clone(),
            None
        )
        .is_err());
    assert!(!dir.path().join("not-allowed").exists());
    let mut m = manifest();
    m["remix"] = json!({"allowed":true,"license":"MIT"});
    source(Path::new(&req.source_path), &m);
    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    lib.publish(req).unwrap();
    let remixed = lib
        .remix(
            &first.record.app_id,
            2,
            &dir.path().join("remix"),
            authoring,
            Some("My copy".into()),
        )
        .unwrap();
    assert_ne!(remixed.record.app_id, first.record.app_id);
    assert_eq!(remixed.record.name, "My copy");
    assert!(remixed.record.execution_grants.is_empty());
    assert_eq!(lib.get(&first.record.app_id).unwrap().current_revision, 1);
}
#[test]
fn market_metadata_changes_only_with_activated_release() {
    let (_dir, lib, mut req) = setup();
    let origin = MarketOrigin {
        app_id: "market-app".into(),
        release_id: "v1".into(),
        digest: "a".repeat(64),
        remixed_from: None,
    };
    req.market_origin = Some(origin.clone());
    let first = lib.publish(req.clone()).unwrap();
    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    req.market_origin.as_mut().unwrap().release_id = "v2".into();
    let second = lib.publish(req).unwrap();
    assert_eq!(second.record.market_origin.unwrap().release_id, "v1");
    assert_eq!(second.release.market_origin.unwrap().release_id, "v2");
    assert_eq!(
        lib.activate_release(&first.record.app_id, 2)
            .unwrap()
            .market_origin
            .unwrap()
            .release_id,
        "v2"
    );
}

#[test]
fn prepared_environment_is_reused_but_repair_gets_an_independent_workspace() {
    let (_dir, library, request) = setup();
    let app = library.publish(request).unwrap().record.app_id;
    let first = library.prepare_run(&app, 1).unwrap();
    assert!(!first.environment_prepared);
    fs::write(
        Path::new(&first.work_path).join("dependency-installed"),
        "offline ready",
    )
    .unwrap();
    let mut ready = first.run.clone();
    ready.phase = RunPhase::Running;
    library.update_run(ready).unwrap();
    let resumed = library.prepare_run(&app, 1).unwrap();
    assert!(resumed.environment_prepared);
    assert_ne!(first.run.run_id, resumed.run.run_id);
    assert_eq!(first.work_path, resumed.work_path);
    assert_eq!(
        fs::read_to_string(Path::new(&resumed.work_path).join("dependency-installed")).unwrap(),
        "offline ready"
    );
    let repair = library.prepare_run_with_options(&app, 1, true).unwrap();
    assert!(!repair.environment_prepared);
    assert_ne!(repair.work_path, first.work_path);
    assert!(!Path::new(&repair.work_path)
        .join("dependency-installed")
        .exists());
}

#[test]
fn remix_records_the_selected_release_origin_even_before_activation() {
    let (dir, library, mut request) = setup();
    let mut value = manifest();
    value["remix"] = json!({"allowed":true,"license":"MIT"});
    source(Path::new(&request.source_path), &value);
    request.market_origin = Some(MarketOrigin {
        app_id: "market-app".into(),
        release_id: "v1".into(),
        digest: "a".repeat(64),
        remixed_from: None,
    });
    let first = library.publish(request.clone()).unwrap();
    request.app_id = Some(first.record.app_id.clone());
    request.expected_revision = 1;
    request.market_origin.as_mut().unwrap().release_id = "v2".into();
    library.publish(request).unwrap();
    assert_eq!(
        library
            .get(&first.record.app_id)
            .unwrap()
            .market_origin
            .unwrap()
            .release_id,
        "v1"
    );
    let copied = library
        .remix(
            &first.record.app_id,
            2,
            &dir.path().join("copy"),
            authoring("/home/mobile-user/.lingxi/.apps/copied"),
            None,
        )
        .unwrap();
    let origin = copied.record.market_origin.unwrap();
    assert_eq!(origin.release_id, "v2");
    assert_eq!(origin.remixed_from.as_deref(), Some("market-app"));
}
#[test]
fn removed_installation_origin_is_never_reused() {
    let (_dir, lib, req) = setup();
    let first = lib.publish(req.clone()).unwrap();
    lib.remove(&first.record.app_id).unwrap();
    let second = lib.publish(req).unwrap();
    assert_ne!(first.record.origin_port, second.record.origin_port);
}
#[cfg(unix)]
#[test]
fn executable_mode_is_preserved_and_bound_to_digest() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, lib, req) = setup();
    let mut m = manifest();
    m["package"]["files"] = json!(["index.html", "install.sh"]);
    m["install"] = json!([{"command":"./install.sh"}]);
    source(Path::new(&req.source_path), &m);
    let script = Path::new(&req.source_path).join("install.sh");
    fs::write(&script, "#!/bin/sh\nprintf ready").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o744)).unwrap();
    let first = lib.publish(req).unwrap();
    assert!(
        first
            .release
            .files
            .iter()
            .find(|f| f.path == "install.sh")
            .unwrap()
            .executable
    );
    lib.grant_execution(&first.record.app_id, &first.release.digest)
        .unwrap();
    let run = lib.prepare_run(&first.record.app_id, 1).unwrap();
    assert_ne!(
        fs::metadata(Path::new(&run.work_path).join("install.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    let export = dir.path().join("export");
    lib.export(&first.record.app_id, 1, &export).unwrap();
    assert_ne!(
        fs::metadata(export.join("install.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    fs::set_permissions(
        lib.source_path(&first.record.app_id, 1)
            .unwrap()
            .join("install.sh"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(lib.verify_release(&first.record.app_id, 1).is_err());
}
#[test]
fn interrupted_publication_can_retry_without_overwriting_archived_bytes() {
    let (_dir, lib, mut req) = setup();
    let first = lib.publish(req.clone()).unwrap();
    let releases = lib
        .source_path(&first.record.app_id, 1)
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    fs::create_dir_all(releases.join("2")).unwrap();
    fs::write(releases.join("2/unindexed-source"), "keep orphan").unwrap();
    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    assert_eq!(lib.publish(req).unwrap().record.latest_revision, 2);
    assert!(fs::read_dir(lib.root().join("transactions"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().join("unindexed-source").exists()));
}
#[test]
fn public_export_rejects_credentials_embedded_in_process_environment() {
    let (dir, lib, req) = setup();
    let mut m = manifest();
    m["install"] = json!([{"command":"python setup.py","env":{"OPENAI_API_KEY":"do-not-upload"}}]);
    source(Path::new(&req.source_path), &m);
    let local = lib.publish(req).unwrap();
    assert!(lib
        .export(&local.record.app_id, 1, &dir.path().join("export"))
        .is_err());
    assert!(!dir.path().join("export").exists());
}
#[test]
fn explicit_process_capability_requires_digest_consent_but_agent_does_not() {
    let (_dir, lib, req) = setup();
    let mut m = manifest();
    m["capabilities"] = json!(["process"]);
    source(Path::new(&req.source_path), &m);
    let process = lib.publish(req.clone()).unwrap();
    assert!(process.release.manifest.requires_execution());
    assert!(matches!(
        lib.prepare_run(&process.record.app_id, 1),
        Err(AppError::ConsentRequired(_))
    ));
    lib.grant_execution(&process.record.app_id, &process.release.digest)
        .unwrap();
    assert!(lib.prepare_run(&process.record.app_id, 1).is_ok());
    m["capabilities"] = json!(["agent"]);
    m["agent"] = json!({"tools":["Read"]});
    source(Path::new(&req.source_path), &m);
    let agent = lib.publish(req).unwrap();
    assert!(!agent.release.manifest.requires_execution());
    assert!(lib.prepare_run(&agent.record.app_id, 1).is_ok());
}

#[test]
fn preparation_data_is_separate_from_live_data_and_other_runs() {
    let (_dir, lib, req) = setup();
    let first = lib.publish(req).unwrap();
    let one = lib.prepare_run(&first.record.app_id, 1).unwrap();
    let two = lib.prepare_run(&first.record.app_id, 1).unwrap();
    assert_ne!(one.preparation_data_path, one.data_path);
    assert_ne!(one.preparation_data_path, two.preparation_data_path);
    fs::write(
        Path::new(&one.preparation_data_path).join("database"),
        "candidate",
    )
    .unwrap();
    assert!(!Path::new(&one.data_path).join("database").exists());
    assert!(!Path::new(&two.preparation_data_path)
        .join("database")
        .exists());
}
#[test]
fn service_names_cannot_alias_the_same_port_environment_variable() {
    for names in [
        ["api-one", "api_one"],
        ["api.one", "API_ONE"],
        ["Api", "api"],
    ] {
        let mut m = manifest();
        m["processes"] = json!([{"name":names[0],"command":"node one.js"},{"name":names[1],"command":"node two.js"}]);
        assert!(Manifest::parse(&m.to_string()).is_err());
    }
}
