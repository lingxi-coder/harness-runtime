use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
fn write_uncommitted_pending(lib: &AppLibrary, binding: &AuthoringRef) {
    let name = format!(
        "{:x}.json",
        Sha256::digest(binding.source_directory.as_bytes())
    );
    fs::write(
        lib.root().join("pending-authoring").join(name),
        serde_json::to_vec(binding).unwrap(),
    )
    .unwrap();
}
fn registered_source(
    lib: &AppLibrary,
    source: &str,
    identity: rooted_fs::RootIdentity,
) -> (String, String) {
    let request_id = uuid::Uuid::new_v4().to_string();
    let payload = json!({"op":"testSource","sourceDirectory":source});
    let id = lib
        .create_native_authoring_source(
            &request_id,
            &payload,
            None,
            None,
            || Ok(source.into()),
            |_| Ok(identity),
        )
        .unwrap()
        .0;
    (id, request_id)
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
        authoring: None,
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
fn app_owned_mcp_requires_desktop_targets_and_exact_release_consent() {
    let (_dir, library, request) = setup();
    let mut value = manifest();
    value["targets"] = json!([{"platform":"macos","environment":"native"}]);
    value["capabilities"] = json!(["mcp"]);
    value["mcpServers"] = json!({"tools":{"type":"stdio","command":"node mcp.mjs"}});
    let parsed = Manifest::parse(&value.to_string()).unwrap();
    assert!(parsed.requires_execution());
    source(Path::new(&request.source_path), &value);
    let published = library.publish(request).unwrap();
    let id = &published.record.app_id;
    assert!(matches!(
        library.prepare_run(id, 1),
        Err(AppError::ConsentRequired(_))
    ));
    library
        .grant_execution(id, &published.release.digest)
        .unwrap();
    assert!(library.prepare_run(id, 1).is_ok());
    library.revoke_execution(id).unwrap();
    assert!(matches!(
        library.require_execution_grant(id, 1),
        Err(AppError::ConsentRequired(_))
    ));
    value["targets"] = json!([{"platform":"ios","environment":"mobile-linux"}]);
    assert!(Manifest::parse(&value.to_string()).is_err());
    value["targets"] = json!([{"platform":"macos","environment":"native"}]);
    value["capabilities"] = json!([]);
    assert!(Manifest::parse(&value.to_string()).is_err());
}
#[test]
fn app_owned_mcp_endpoints_are_process_bound_and_public_exports_exclude_secrets() {
    let mut value = manifest();
    value["targets"] = json!([{"platform":"macos","environment":"native"}]);
    value["capabilities"] = json!(["mcp"]);
    value["processes"] = json!([{"name":"api","command":"node server.mjs"}]);
    value["mcpServers"] = json!({"tools":{"type":"http","process":"api","path":"/mcp"}});
    assert!(Manifest::parse(&value.to_string()).is_ok());
    for path in ["//remote/mcp", "/%2e%2e/mcp", "/a/../mcp", "/mcp?token=x"] {
        value["mcpServers"]["tools"]["path"] = json!(path);
        assert!(Manifest::parse(&value.to_string()).is_err());
    }
    value["mcpServers"] = json!({"tools":{"type":"http","process":"missing","path":"/mcp"}});
    assert!(Manifest::parse(&value.to_string()).is_err());
    value["mcpServers"] =
        json!({"tools":{"type":"stdio","command":"node mcp.mjs","env":{"API_KEY":"secret"}}});
    assert!(Manifest::parse(&value.to_string())
        .unwrap()
        .validate_public_export()
        .is_err());
    value["mcpServers"]["tools"]["url"] = json!("https://example.com");
    assert!(Manifest::parse(&value.to_string()).is_err());
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
    let (dir, lib, req) = setup();
    let source = "/home/user/.lingxi/.apps/test";
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let (source_id, request_id) = registered_source(&lib, source, identity);
    let binding = authoring(source);
    let session = binding.authoring_session_id.clone().unwrap();
    lib.register_native_authoring(binding, &source_id, &request_id, true, |_| Ok(identity))
        .unwrap();
    let first = lib
        .publish_with_native_authoring(req, source, &session, None, None, true, |_| Ok(identity))
        .unwrap();
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
fn native_authoring_requires_a_complete_canonical_binding() {
    let (dir, lib, _req) = setup();
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let (source_id, request_id) = registered_source(&lib, source, identity);
    assert_eq!(
        lib.prepare_native_authoring_source(source, Some(&source_id), || Ok(identity))
            .unwrap(),
        source_id
    );
    assert!(lib
        .prepare_native_authoring_source(source, Some(&uuid::Uuid::new_v4().to_string()), || Ok(
            identity
        ))
        .is_err());
    assert!(matches!(
        lib.prepare_native_authoring_source(
            "/home/user/.lingxi/.apps/absent",
            Some(&source_id),
            || Ok(identity)
        ),
        Err(AppError::NotFound(_))
    ));
    let mut incomplete = authoring(source);
    incomplete.authoring_session_id = None;
    assert!(lib
        .register_native_authoring(incomplete, &source_id, &request_id, true, |_| Ok(identity))
        .is_err());
    assert!(lib
        .register_native_authoring(
            authoring(source),
            &uuid::Uuid::new_v4().to_string(),
            &request_id,
            true,
            |_| Ok(identity)
        )
        .is_err());

    let mut already_owned = authoring(source);
    already_owned.app_id = Some("a00000000000000000000000000000000".into());
    assert!(lib
        .register_native_authoring(already_owned, &source_id, &request_id, true, |_| Ok(
            identity
        ))
        .is_err());

    let mut noncanonical = authoring("/home/user/.lingxi/.apps/../outside");
    assert!(lib
        .register_native_authoring(noncanonical.clone(), &source_id, &request_id, true, |_| Ok(
            identity
        ))
        .is_err());
    noncanonical.source_directory = source.into();
    let saved = lib
        .register_native_authoring(noncanonical.clone(), &source_id, &request_id, true, |_| {
            Ok(identity)
        })
        .unwrap();
    assert_eq!(saved, noncanonical);
}

#[test]
fn native_source_creation_request_is_durable_idempotent_and_bound_to_physical_source() {
    let (dir, lib, _) = setup();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let request = uuid::Uuid::new_v4().to_string();
    let payload = json!({"op":"createAuthoring","name":"Notes"});
    let created = lib
        .create_native_authoring_source(
            &request,
            &payload,
            None,
            None,
            || Ok(source.into()),
            |_| Ok(identity),
        )
        .unwrap();
    let retried = lib
        .create_native_authoring_source(
            &request,
            &payload,
            None,
            None,
            || panic!("must not recreate committed source"),
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(created, retried);
    assert!(lib
        .create_native_authoring_source(
            &request,
            &json!({"op":"createAuthoring","name":"Other"}),
            None,
            None,
            || panic!("conflict"),
            |_| Ok(identity)
        )
        .is_err());
    let other = tempfile::tempdir().unwrap();
    let replacement = rooted_fs::root_identity(other.path()).unwrap();
    assert!(lib
        .create_native_authoring_source(
            &request,
            &payload,
            None,
            None,
            || panic!("replaced"),
            |_| Ok(replacement)
        )
        .is_err());
    let second_request = uuid::Uuid::new_v4().to_string();
    let same = lib
        .create_native_authoring_source(
            &second_request,
            &payload,
            None,
            None,
            || Ok(source.into()),
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(same, created);
}

#[test]
fn mobile_authoring_receipts_survive_publication_rebind_and_reject_conflicts() {
    let (dir, lib, mut req) = setup();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let (source_id, request_id) = registered_source(&lib, source, identity);
    let first = authoring(source);
    let ProjectRef::MobileProject { project_id } = first.project_ref.as_ref().unwrap() else {
        unreachable!()
    };
    let session = first.authoring_session_id.as_deref().unwrap();
    let get = |project: &str, session: &str| {
        lib.resolve_native_authoring(
            source,
            &ProjectRef::MobileProject {
                project_id: project.into(),
            },
            session,
            |_| Ok(identity),
        )
    };
    assert!(matches!(
        get(project_id, session),
        Err(AppError::NotFound(_))
    ));
    // A pointer alone is not a grant, even after a partial registration.
    write_uncommitted_pending(&lib, &first);
    assert!(matches!(
        get(project_id, session),
        Err(AppError::NotFound(_))
    ));
    let mut partial_publish = req.clone();
    partial_publish.authoring = None;
    assert!(lib
        .publish_with_native_authoring(partial_publish, source, session, None, None, true, |_| Ok(
            identity
        ))
        .is_err());
    assert_eq!(
        lib.register_native_authoring(first.clone(), &source_id, &request_id, true, |_| Ok(
            identity
        ))
        .unwrap(),
        first
    );
    assert_eq!(
        lib.register_native_authoring(first.clone(), &source_id, &request_id, true, |_| Ok(
            identity
        ))
        .unwrap(),
        first
    );
    assert_eq!(get(project_id, session).unwrap(), first);
    let mut conflicting = first.clone();
    conflicting.workspace_id = Some(uuid::Uuid::new_v4().to_string());
    assert!(lib
        .register_native_authoring(conflicting, &source_id, &request_id, true, |_| Ok(identity))
        .is_err());
    assert!(matches!(
        get(&uuid::Uuid::new_v4().to_string(), session),
        Err(AppError::NotFound(_))
    ));
    let other = tempfile::tempdir().unwrap();
    let replacement = rooted_fs::root_identity(other.path()).unwrap();
    assert!(lib
        .resolve_native_authoring(
            source,
            &ProjectRef::MobileProject {
                project_id: project_id.clone()
            },
            session,
            |_| Ok(replacement)
        )
        .is_err());

    // A second confirmed session is independent; publication consumes only
    // its current pending pointer, never either immutable owner receipt.
    let second = authoring(source);
    let ProjectRef::MobileProject {
        project_id: second_project,
    } = second.project_ref.as_ref().unwrap()
    else {
        unreachable!()
    };
    let second_session = second.authoring_session_id.as_deref().unwrap();
    lib.register_native_authoring(second.clone(), &source_id, &request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    req.authoring = None;
    let digest = lib
        .fingerprint_publication(Path::new(&req.source_path))
        .unwrap();
    let publication_id = uuid::Uuid::new_v4().to_string();
    let published = lib
        .publish_native_authoring_once(
            &publication_id,
            &digest,
            req,
            source,
            session,
            first.project_ref.as_ref(),
            Some(&source_id),
            true,
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(
        published
            .record
            .authoring
            .as_ref()
            .unwrap()
            .authoring_session_id
            .as_deref(),
        Some(second_session)
    );
    let (receipt, release) = lib
        .native_authoring_publication(&publication_id)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.publisher.as_ref(), Some(&first));
    assert_eq!(receipt.source_id.as_deref(), Some(source_id.as_str()));
    assert_eq!(release.release.digest, digest);
    assert_eq!(get(project_id, session).unwrap(), first);
    assert_eq!(get(second_project, second_session).unwrap(), second);
    let mut second_current = second.clone();
    second_current.app_id = Some(published.record.app_id.clone());
    lib.register_native_authoring(second_current, &source_id, &request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    let mut recovering = authoring(source);
    recovering.app_id = Some(published.record.app_id.clone());
    lib.register_native_authoring(recovering.clone(), &source_id, &request_id, false, |_| {
        Ok(identity)
    })
    .unwrap();
    assert_eq!(
        lib.get(&published.record.app_id)
            .unwrap()
            .authoring
            .unwrap()
            .authoring_session_id
            .as_deref(),
        Some(second_session)
    );
    let ProjectRef::MobileProject {
        project_id: recovered_project,
    } = recovering.project_ref.as_ref().unwrap()
    else {
        unreachable!()
    };
    assert_eq!(
        get(
            recovered_project,
            recovering.authoring_session_id.as_deref().unwrap()
        )
        .unwrap(),
        recovering
    );

    // Rebinding the current pointer does not revoke either old receipt, and
    // the same owner keeps its original optional appId even after publication.
    let mut rebound = first.clone();
    rebound.app_id = Some(published.record.app_id.clone());
    assert_eq!(
        lib.register_native_authoring(rebound, &source_id, &request_id, true, |_| Ok(identity))
            .unwrap(),
        first
    );
    assert_eq!(get(project_id, session).unwrap(), first);
    assert_eq!(get(second_project, second_session).unwrap(), second);
    assert_eq!(
        lib.get(&published.record.app_id)
            .unwrap()
            .authoring
            .unwrap()
            .authoring_session_id
            .as_deref(),
        Some(session)
    );
}

#[test]
fn late_source_intent_repairs_owner_and_affinity_without_replacing_newer_current() {
    let (dir, lib, mut req) = setup();
    req.authoring = None;
    let app = lib.publish(req).unwrap().record;
    let source_a = "/home/user/.lingxi/.apps/a";
    let source_b = "/home/user/.lingxi/.apps/b";
    let a_path = dir.path().join("physical-a");
    let b_path = dir.path().join("physical-b");
    fs::create_dir(&a_path).unwrap();
    fs::create_dir(&b_path).unwrap();
    let a_identity = rooted_fs::root_identity(&a_path).unwrap();
    let b_identity = rooted_fs::root_identity(&b_path).unwrap();
    let a_request = uuid::Uuid::new_v4().to_string();
    let b_request = uuid::Uuid::new_v4().to_string();
    let a_payload = json!({"op":"bindSource","targetAppId":app.app_id,"sourceDirectory":source_a});
    let b_payload = json!({"op":"bindSource","targetAppId":app.app_id,"sourceDirectory":source_b});
    let a_id = lib
        .create_native_authoring_source(
            &a_request,
            &a_payload,
            Some(&app.app_id),
            None,
            || Ok(source_a.into()),
            |_| Ok(a_identity),
        )
        .unwrap()
        .0;
    let b_id = lib
        .create_native_authoring_source(
            &b_request,
            &b_payload,
            Some(&app.app_id),
            None,
            || Ok(source_b.into()),
            |_| Ok(b_identity),
        )
        .unwrap()
        .0;
    let mut a = authoring(source_a);
    a.app_id = Some(app.app_id.clone());
    let mut b = authoring(source_b);
    b.app_id = Some(app.app_id.clone());
    assert!(lib
        .register_native_authoring(a.clone(), &a_id, &a_request, true, |_| Err(
            AppError::Invalid("pre-write failure".into())
        ))
        .is_err());
    lib.register_native_authoring(b.clone(), &b_id, &b_request, true, |_| Ok(b_identity))
        .unwrap();
    lib.register_native_authoring(a.clone(), &a_id, &a_request, false, |_| Ok(a_identity))
        .unwrap();
    let current = lib.get(&app.app_id).unwrap();
    assert_eq!(current.authoring, Some(b));
    assert!(current.authoring_source_ids.contains(&a_id));
    assert!(current.authoring_source_ids.contains(&b_id));
    let project = a.project_ref.as_ref().unwrap();
    assert_eq!(
        lib.resolve_native_authoring(
            source_a,
            project,
            a.authoring_session_id.as_deref().unwrap(),
            |_| Ok(a_identity)
        )
        .unwrap(),
        a
    );
}

#[cfg(unix)]
#[test]
fn mobile_authoring_receipt_rejects_symlink_and_same_path_replacement() {
    use std::os::unix::fs::symlink;
    let (dir, lib, _) = setup();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let source_path = dir.path().join("source");
    fs::create_dir(&source_path).unwrap();
    let identity = rooted_fs::root_identity(&source_path).unwrap();
    let (source_id, request_id) = registered_source(&lib, source, identity);
    let binding = authoring(source);
    let ProjectRef::MobileProject { project_id } = binding.project_ref.as_ref().unwrap() else {
        unreachable!()
    };
    let session = binding.authoring_session_id.as_deref().unwrap();
    lib.register_native_authoring(binding.clone(), &source_id, &request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    fs::rename(&source_path, dir.path().join("old-source")).unwrap();
    fs::create_dir(&source_path).unwrap();
    let replacement = rooted_fs::root_identity(&source_path).unwrap();
    assert_ne!(identity, replacement);
    assert!(lib
        .resolve_native_authoring(
            source,
            &ProjectRef::MobileProject {
                project_id: project_id.clone()
            },
            session,
            |_| Ok(replacement)
        )
        .is_err());
    let entry = fs::read_dir(lib.root().join("pending-authoring"))
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_name().to_string_lossy().starts_with("owner-"))
        .unwrap()
        .path();
    let saved = entry.with_extension("saved");
    fs::rename(&entry, &saved).unwrap();
    symlink(&saved, &entry).unwrap();
    assert!(lib
        .resolve_native_authoring(
            source,
            &ProjectRef::MobileProject {
                project_id: project_id.clone()
            },
            session,
            |_| Ok(identity)
        )
        .is_err());
}

#[test]
fn mobile_authoring_receipt_holds_library_lock_through_validation() {
    use std::{sync::mpsc, time::Duration};
    let (dir, lib, _) = setup();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let (source_id, request_id) = registered_source(&lib, source, identity);
    let first = authoring(source);
    let ProjectRef::MobileProject { project_id } = first.project_ref.as_ref().unwrap() else {
        unreachable!()
    };
    let session = first.authoring_session_id.as_deref().unwrap();
    lib.register_native_authoring(first.clone(), &source_id, &request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let second = authoring(source);
    std::thread::scope(|scope| {
        let lookup_lib = &lib;
        let lookup = scope.spawn(move || {
            lookup_lib.resolve_native_authoring(
                source,
                &ProjectRef::MobileProject {
                    project_id: project_id.clone(),
                },
                session,
                move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(identity)
                },
            )
        });
        entered_rx.recv().unwrap();
        let change_lib = &lib;
        let change = scope.spawn(move || {
            attempt_tx.send(()).unwrap();
            let result =
                change_lib.register_native_authoring(second, &source_id, &request_id, true, |_| {
                    Ok(identity)
                });
            done_tx.send(()).unwrap();
            result
        });
        attempt_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        release_tx.send(()).unwrap();
        assert_eq!(lookup.join().unwrap().unwrap(), first);
        change.join().unwrap().unwrap();
    });
    assert_eq!(
        lib.resolve_native_authoring(
            source,
            &ProjectRef::MobileProject {
                project_id: project_id.clone()
            },
            session,
            |_| Ok(identity)
        )
        .unwrap(),
        first
    );
}

#[test]
fn mobile_publication_rejects_ambiguous_owner_and_rechecks_before_commit() {
    use std::cell::Cell;
    let (dir, lib, mut req) = setup();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let (source_id, request_id) = registered_source(&lib, source, identity);
    let first = authoring(source);
    let session = first.authoring_session_id.clone().unwrap();
    lib.register_native_authoring(first.clone(), &source_id, &request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    req.authoring = None;
    let calls = Cell::new(0);
    let changed =
        lib.publish_with_native_authoring(req.clone(), source, &session, None, None, true, |_| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(identity)
            } else {
                Err(AppError::Invalid("engine changed".into()))
            }
        });
    assert!(changed.is_err());
    assert!(lib.list().unwrap().is_empty());
    let mut second = authoring(source);
    second.authoring_session_id = Some(session.clone());
    lib.register_native_authoring(second, &source_id, &request_id, false, |_| Ok(identity))
        .unwrap();
    assert!(lib
        .publish_with_native_authoring(req, source, &session, None, None, true, |_| Ok(identity))
        .is_err());
    assert!(lib.list().unwrap().is_empty());
}

#[test]
fn cold_authoring_receipt_without_current_pointer_can_publish() {
    let (dir, lib, mut req) = setup();
    let source = "/home/user/.lingxi/.apps/notes-1234abcd";
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let (source_id, request_id) = registered_source(&lib, source, identity);
    let binding = authoring(source);
    let session = binding.authoring_session_id.clone().unwrap();
    lib.register_native_authoring(binding.clone(), &source_id, &request_id, false, |_| {
        Ok(identity)
    })
    .unwrap();
    let pending = lib
        .root()
        .join("pending-authoring")
        .join(format!("{:x}.json", Sha256::digest(source.as_bytes())));
    assert!(!pending.exists());
    let reopened = AppLibrary::open(dir.path().join("config")).unwrap();
    assert_eq!(
        reopened
            .prepare_native_authoring_source(source, Some(&source_id), || Ok(identity))
            .unwrap(),
        source_id
    );
    req.authoring = None;
    let published = reopened
        .publish_with_native_authoring(req, source, &session, None, None, true, |_| Ok(identity))
        .unwrap();
    assert_eq!(
        published
            .record
            .authoring
            .unwrap()
            .authoring_session_id
            .as_deref(),
        Some(session.as_str())
    );
}

#[test]
fn native_publication_replays_committed_outcome_and_fences_payload_changes() {
    let (dir, lib, mut req) = setup();
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let source = "/home/user/.lingxi/.apps/once";
    let (source_id, source_request_id) = registered_source(&lib, source, identity);
    let owner = authoring(source);
    let session = owner.authoring_session_id.clone().unwrap();
    let project = owner.project_ref.clone().unwrap();
    lib.register_native_authoring(owner, &source_id, &source_request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    req.authoring = None;
    let digest = lib
        .fingerprint_publication(Path::new(&req.source_path))
        .unwrap();
    let request_id = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        lib.replay_native_authoring_publication(
            &request_id,
            &digest,
            &req,
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
        ),
        Err(AppError::NotFound(_))
    ));
    assert!(lib.list().unwrap().is_empty());
    let staged_file = Path::new(&req.source_path).join("index.html");
    let original = fs::read(&staged_file).unwrap();
    fs::write(&staged_file, "changed after fingerprint").unwrap();
    assert!(lib
        .publish_native_authoring_once(
            &request_id,
            &digest,
            req.clone(),
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
            |_| Ok(identity)
        )
        .is_err());
    assert!(lib.list().unwrap().is_empty());
    fs::write(&staged_file, original).unwrap();
    let first = lib
        .publish_native_authoring_once(
            &request_id,
            &digest,
            req.clone(),
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(first.release.digest, digest);
    fs::remove_dir_all(&req.source_path).unwrap();
    let replay = lib
        .publish_native_authoring_once(
            &request_id,
            &digest,
            req.clone(),
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    let readonly = lib
        .replay_native_authoring_publication(
            &request_id,
            &digest,
            &req,
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(readonly).unwrap()
    );
    assert!(lib
        .replay_native_authoring_publication(
            &request_id,
            &"0".repeat(64),
            &req,
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
        )
        .is_err());
    assert!(lib
        .publish_native_authoring_once(
            &request_id,
            &"0".repeat(64),
            req.clone(),
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
            |_| Ok(identity)
        )
        .is_err());
    req.expected_revision = 1;
    assert!(lib
        .publish_native_authoring_once(
            &request_id,
            &digest,
            req,
            source,
            &session,
            Some(&project),
            Some(&source_id),
            true,
            |_| Ok(identity)
        )
        .is_err());
    assert_eq!(lib.list().unwrap().len(), 1);
}

#[test]
fn ordinary_publication_replay_only_never_creates_a_missing_receipt() {
    let (_dir, lib, mut request) = setup();
    request.authoring = None;
    let digest = lib
        .fingerprint_publication(Path::new(&request.source_path))
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        lib.replay_publication(&id, &digest, &request, None),
        Err(AppError::NotFound(_))
    ));
    assert!(lib.list().unwrap().is_empty());
    let published = lib
        .publish_once(&id, &digest, request.clone(), None)
        .unwrap();
    fs::remove_dir_all(&request.source_path).unwrap();
    let replay = lib
        .replay_publication(&id, &digest, &request, None)
        .unwrap();
    assert_eq!(
        serde_json::to_value(published).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert!(lib
        .replay_publication(&id, &"0".repeat(64), &request, None)
        .is_err());
    let missing = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        lib.replay_publication(&missing, &digest, &request, None),
        Err(AppError::NotFound(_))
    ));
    assert_eq!(lib.list().unwrap().len(), 1);
}

#[test]
fn publication_records_actual_owner_without_rebinding_preferred_authoring() {
    let (dir, lib, mut req) = setup();
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let source = "/home/user/.lingxi/.apps/preferred";
    let (source_id, source_request_id) = registered_source(&lib, source, identity);
    let a = authoring(source);
    let b = authoring(source);
    let session_a = a.authoring_session_id.clone().unwrap();
    lib.register_native_authoring(a.clone(), &source_id, &source_request_id, false, |_| {
        Ok(identity)
    })
    .unwrap();
    lib.register_native_authoring(b.clone(), &source_id, &source_request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    req.authoring = None;
    let digest = lib
        .fingerprint_publication(Path::new(&req.source_path))
        .unwrap();
    let first_id = uuid::Uuid::new_v4().to_string();
    let first = lib
        .publish_native_authoring_once(
            &first_id,
            &digest,
            req.clone(),
            source,
            &session_a,
            a.project_ref.as_ref(),
            Some(&source_id),
            true,
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(
        first
            .record
            .authoring
            .as_ref()
            .unwrap()
            .authoring_session_id,
        b.authoring_session_id
    );
    let (receipt, _) = lib
        .native_authoring_publication(&first_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.publisher.unwrap().authoring_session_id,
        a.authoring_session_id
    );
    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    let next_id = uuid::Uuid::new_v4().to_string();
    let next = lib
        .publish_native_authoring_once(
            &next_id,
            &digest,
            req,
            source,
            &session_a,
            a.project_ref.as_ref(),
            Some(&source_id),
            true,
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(next.record.authoring, first.record.authoring);
    let (receipt, _) = lib.native_authoring_publication(&next_id).unwrap().unwrap();
    assert_eq!(
        receipt.publisher.unwrap().authoring_session_id,
        a.authoring_session_id
    );
}

#[test]
fn native_source_restore_is_idempotent_and_preserves_current_owner() {
    let (dir, lib, req) = setup();
    let published = lib.publish(req).unwrap();
    let app_id = published.record.app_id.clone();
    let destination = dir.path().canonicalize().unwrap().join("restored-native");
    let source = destination.to_str().unwrap().to_owned();
    let request_id = uuid::Uuid::new_v4().to_string();
    let payload =
        json!({"op":"copyAuthoringSource","appId":app_id,"revision":1,"sourcePath":source});
    let restore = || {
        lib.create_native_authoring_source(
            &request_id,
            &payload,
            Some(&app_id),
            None,
            || {
                lib.copy_native_authoring_source(&app_id, 1, &destination)?;
                Ok(source.clone())
            },
            native_source_identity,
        )
    };
    let first = restore().unwrap();
    assert_eq!(restore().unwrap(), first);
    assert_eq!(
        lib.get(&app_id).unwrap().authoring,
        published.record.authoring
    );
    assert_eq!(
        fs::read(destination.join("index.html")).unwrap(),
        b"<!doctype html><html>one</html>"
    );
    let conflicting =
        json!({"op":"copyAuthoringSource","appId":app_id,"revision":2,"sourcePath":source});
    assert!(lib
        .create_native_authoring_source(
            &request_id,
            &conflicting,
            Some(&app_id),
            None,
            || unreachable!(),
            native_source_identity
        )
        .is_err());
}

#[test]
fn native_remix_keeps_source_affinity_and_replays_without_the_source() {
    let (dir, lib, mut request) = setup();
    let mut original_manifest = manifest();
    original_manifest["remix"] = json!({"allowed":true,"license":"MIT"});
    source(Path::new(&request.source_path), &original_manifest);
    request.market_origin = Some(MarketOrigin {
        app_id: "market-app".into(),
        release_id: "release-1".into(),
        digest: "a".repeat(64),
        remixed_from: None,
    });
    let original = lib.publish(request).unwrap();
    let app_id = &original.record.app_id;
    let destination = dir.path().canonicalize().unwrap().join("remix");
    let source = destination.to_str().unwrap();
    let request_id = uuid::Uuid::new_v4().to_string();
    let payload = json!({"op":"remixAuthoringSource","appId":app_id,"revision":1,
        "sourcePath":source,"name":"My copy"});
    let (expected_digest, origin) = lib
        .native_remix_source_metadata(app_id, 1, Some("My copy"))
        .unwrap();
    let (source_id, actual_source) = lib
        .create_native_authoring_source(
            &request_id,
            &payload,
            None,
            None,
            || {
                lib.copy_native_remix_source(app_id, 1, &destination, Some("My copy"))?;
                Ok(source.into())
            },
            native_source_identity,
        )
        .unwrap();
    assert_eq!(actual_source, source);
    assert_eq!(
        lib.get(app_id).unwrap().authoring,
        original.record.authoring
    );
    let result = lib
        .publish_native_remix_once(
            &request_id,
            &payload,
            &source_id,
            source,
            &destination,
            &expected_digest,
            origin,
            native_source_identity,
        )
        .unwrap();
    assert_ne!(result.record.app_id, *app_id);
    assert!(result.record.authoring.is_none());
    assert_eq!(result.record.authoring_source_ids, vec![source_id.clone()]);
    assert_eq!(result.record.name, "My copy");
    assert_eq!(
        result
            .record
            .market_origin
            .as_ref()
            .unwrap()
            .remixed_from
            .as_deref(),
        Some("market-app")
    );
    let mut binding = authoring(source);
    binding.app_id = Some(result.record.app_id.clone());
    lib.register_native_authoring(binding.clone(), &source_id, &request_id, false, |value| {
        native_source_identity(&value.source_directory)
    })
    .unwrap();
    assert!(lib.get(&result.record.app_id).unwrap().authoring.is_none());
    let mut wrong = binding;
    wrong.app_id = Some(app_id.clone());
    assert!(lib
        .register_native_authoring(wrong, &source_id, &request_id, false, |value| {
            native_source_identity(&value.source_directory)
        })
        .is_err());
    std::fs::remove_dir_all(&destination).unwrap();
    let replay = lib
        .replay_native_remix(&request_id, &payload)
        .unwrap()
        .unwrap();
    assert_eq!(replay.0, source_id);
    assert_eq!(replay.1, source);
    assert_eq!(replay.2.record.app_id, result.record.app_id);
    let mut changed = payload;
    changed["revision"] = json!(2);
    assert!(lib.replay_native_remix(&request_id, &changed).is_err());
    assert_eq!(
        lib.get(app_id).unwrap().authoring,
        original.record.authoring
    );
}

#[test]
fn native_remix_requires_a_license_and_uses_the_selected_release_origin() {
    let (dir, lib, mut request) = setup();
    request.market_origin = Some(MarketOrigin {
        app_id: "market-app".into(),
        release_id: "v1".into(),
        digest: "a".repeat(64),
        remixed_from: None,
    });
    let original = lib.publish(request.clone()).unwrap();
    let destination = dir.path().join("remix-without-license");
    assert!(lib
        .native_remix_source_metadata(&original.record.app_id, 1, None)
        .is_err());
    assert!(lib
        .copy_native_remix_source(&original.record.app_id, 1, &destination, None)
        .is_err());
    assert!(!destination.exists());

    let mut manifest = manifest();
    manifest["remix"] = json!({"allowed":true,"license":"MIT"});
    source(Path::new(&request.source_path), &manifest);
    request.app_id = Some(original.record.app_id.clone());
    request.expected_revision = 1;
    request.market_origin.as_mut().unwrap().release_id = "v2".into();
    lib.publish(request).unwrap();
    assert_eq!(
        lib.get(&original.record.app_id)
            .unwrap()
            .market_origin
            .unwrap()
            .release_id,
        "v1"
    );
    let (_, origin) = lib
        .native_remix_source_metadata(&original.record.app_id, 2, None)
        .unwrap();
    let origin = origin.unwrap();
    assert_eq!(origin.release_id, "v2");
    assert_eq!(origin.remixed_from.as_deref(), Some("market-app"));
}

#[test]
fn deleted_app_keeps_source_affinity_but_a_new_source_can_be_published() {
    let (dir, lib, mut request) = setup();
    request.authoring = None;
    let source = Path::new(&request.source_path).canonicalize().unwrap();
    let source_text = source.to_str().unwrap();
    let creation = uuid::Uuid::new_v4().to_string();
    let payload = json!({"op":"createSource","sourceDirectory":source_text});
    let source_id = lib
        .create_native_authoring_source(
            &creation,
            &payload,
            None,
            None,
            || Ok(source_text.into()),
            native_source_identity,
        )
        .unwrap()
        .0;
    let owner = |path: &str| AuthoringRef {
        source_directory: path.into(),
        workspace_id: Some(uuid::Uuid::new_v4().to_string()),
        project_ref: Some(ProjectRef::DesktopPath { path: path.into() }),
        authoring_session_id: Some(uuid::Uuid::new_v4().to_string()),
        app_id: None,
    };
    let first_owner = owner(source_text);
    lib.register_native_authoring(first_owner.clone(), &source_id, &creation, true, |value| {
        native_source_identity(&value.source_directory)
    })
    .unwrap();
    let digest = lib.fingerprint_publication(&source).unwrap();
    let first = lib
        .publish_native_authoring_once(
            &uuid::Uuid::new_v4().to_string(),
            &digest,
            request.clone(),
            source_text,
            first_owner.authoring_session_id.as_deref().unwrap(),
            first_owner.project_ref.as_ref(),
            Some(&source_id),
            true,
            |value| native_source_identity(&value.source_directory),
        )
        .unwrap();
    lib.remove(&first.record.app_id).unwrap();
    let second_owner = owner(source_text);
    assert!(lib
        .register_native_authoring(second_owner.clone(), &source_id, &creation, true, |value| {
            native_source_identity(&value.source_directory)
        })
        .is_err());
    assert!(lib
        .publish_native_authoring_once(
            &uuid::Uuid::new_v4().to_string(),
            &digest,
            request.clone(),
            source_text,
            first_owner.authoring_session_id.as_deref().unwrap(),
            first_owner.project_ref.as_ref(),
            Some(&source_id),
            true,
            |value| native_source_identity(&value.source_directory)
        )
        .is_err());

    let copy = dir.path().canonicalize().unwrap().join("new-source");
    let copy_text = copy.to_str().unwrap();
    let new_creation = uuid::Uuid::new_v4().to_string();
    let new_payload = json!({"op":"createSource","sourceDirectory":copy_text});
    let new_id = lib
        .create_native_authoring_source(
            &new_creation,
            &new_payload,
            None,
            None,
            || {
                std::fs::create_dir(&copy)?;
                for name in ["index.html", "lingxi-app.json"] {
                    std::fs::copy(source.join(name), copy.join(name))?;
                }
                Ok(copy_text.into())
            },
            native_source_identity,
        )
        .unwrap()
        .0;
    assert_ne!(new_id, source_id);
    let new_owner = owner(copy_text);
    lib.register_native_authoring(new_owner.clone(), &new_id, &new_creation, true, |value| {
        native_source_identity(&value.source_directory)
    })
    .unwrap();
    request.source_path = copy_text.into();
    let copied = lib
        .publish_native_authoring_once(
            &uuid::Uuid::new_v4().to_string(),
            &lib.fingerprint_publication(&copy).unwrap(),
            request,
            copy_text,
            new_owner.authoring_session_id.as_deref().unwrap(),
            new_owner.project_ref.as_ref(),
            Some(&new_id),
            true,
            |value| native_source_identity(&value.source_directory),
        )
        .unwrap();
    assert_ne!(copied.record.app_id, first.record.app_id);
    assert!(source.exists());
    let tombstone: AppRecord = serde_json::from_slice(
        &std::fs::read(
            lib.root()
                .join("apps")
                .join(&first.record.app_id)
                .join("record.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(tombstone.removed);
    assert_eq!(tombstone.authoring_source_ids, vec![source_id]);
}

#[test]
fn generic_ingress_rejects_owner_fields_and_registered_source_descendants() {
    let (dir, lib, mut request) = setup();
    let source = Path::new(&request.source_path).canonicalize().unwrap();
    let source_text = source.to_str().unwrap();
    let owner = authoring(source_text);
    let original_manifest = fs::read(source.join("lingxi-app.json")).unwrap();
    let mut direct = request.clone();
    direct.authoring = Some(owner.clone());
    assert!(lib.publish(direct).is_err());
    assert!(lib.list().unwrap().is_empty());
    assert_eq!(
        fs::read(source.join("lingxi-app.json")).unwrap(),
        original_manifest
    );
    let publish =
        json!({"op":"publish","sourcePath":source_text,"expectedRevision":0,"authoring":owner});
    let import = json!({"op":"import","sourcePath":source_text,"authoring":owner});
    assert!(lib.request_json(&publish.to_string()).is_err());
    assert!(lib.request_json(&import.to_string()).is_err());
    for op in ["bindAuthoring", "restoreAuthoring", "remix"] {
        assert!(serde_json::from_value::<LibraryRequest>(json!({"op":op})).is_err());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let proof = json!({"op":"testSource","sourceDirectory":source_text});
    lib.create_native_authoring_source(
        &id,
        &proof,
        None,
        None,
        || Ok(source_text.into()),
        native_source_identity,
    )
    .unwrap();
    let mut direct = request.clone();
    direct.authoring = None;
    assert!(lib.publish(direct).is_err());
    let child = source.join("nested");
    self::source(&child, &manifest());
    let ordinary = dir.path().join("ordinary");
    self::source(&ordinary, &manifest());
    let mut staged = request.clone();
    staged.source_path = ordinary.to_string_lossy().into_owned();
    let digest = lib.fingerprint_publication(&ordinary).unwrap();
    for path in [&source, &child] {
        assert!(lib
            .request_json(&json!({"op":"import","sourcePath":path}).to_string())
            .is_err());
        assert!(lib
            .request_json(
                &json!({"op":"publish","sourcePath":path,"expectedRevision":0}).to_string()
            )
            .is_err());
        assert!(lib
            .publish_once(
                &uuid::Uuid::new_v4().to_string(),
                &digest,
                staged.clone(),
                Some(path.to_str().unwrap()),
            )
            .is_err());
    }
    assert!(lib.list().unwrap().is_empty());
    request.source_path = ordinary.to_string_lossy().into_owned();
    let imported = lib
        .request_json(&json!({"op":"import","sourcePath":request.source_path}).to_string())
        .unwrap();
    assert_eq!(imported["record"]["name"], "Test app");
    assert!(imported["record"].get("authoring").is_none());
}

#[cfg(windows)]
#[test]
fn windows_registered_volume_root_protects_its_descendants() {
    let (_dir, lib, request) = setup();
    let canonical_source = fs::canonicalize(&request.source_path).unwrap();
    let root = canonical_source.ancestors().last().unwrap();
    let root_text = root.to_str().unwrap();
    let request_id = uuid::Uuid::new_v4().to_string();
    lib.create_native_authoring_source(
        &request_id,
        &json!({"op":"testSource","sourceDirectory":root_text}),
        None,
        None,
        || Ok(root_text.into()),
        native_source_identity,
    )
    .unwrap();
    assert!(lib.publish(request).is_err());
    assert!(lib.list().unwrap().is_empty());
}

#[test]
fn windows_source_proof_keys_normalize_verbatim_drive_and_unc_paths() {
    use super::library::windows_source_key;
    assert_eq!(
        windows_source_key(r"C:\Users\Owner\Project"),
        windows_source_key(r"\\?\c:\users\owner\project")
    );
    assert_eq!(
        windows_source_key(r"\\Server\Share\Project"),
        windows_source_key(r"\\?\UNC\server\share\project")
    );
    assert_ne!(
        windows_source_key(r"C:\Users\Owner\Project"),
        windows_source_key(r"D:\Users\Owner\Project")
    );
}

#[cfg(windows)]
#[test]
fn windows_generic_ingress_rejects_canonical_aliases_of_a_registered_source() {
    let (_dir, lib, mut request) = setup();
    let source = Path::new(&request.source_path);
    let original = source.to_str().unwrap();
    assert!(!original.starts_with(r"\\?\"));
    let request_id = uuid::Uuid::new_v4().to_string();
    lib.create_native_authoring_source(
        &request_id,
        &json!({"op":"testSource","sourceDirectory":original}),
        None,
        None,
        || Ok(original.into()),
        native_source_identity,
    )
    .unwrap();
    let canonical = fs::canonicalize(source).unwrap();
    assert!(canonical.to_str().unwrap().starts_with(r"\\?\"));
    request.source_path = canonical.to_string_lossy().into_owned();
    assert!(lib.publish(request.clone()).is_err());
    assert!(lib
        .request_json(&json!({"op":"import","sourcePath":request.source_path}).to_string())
        .is_err());
    assert!(lib.list().unwrap().is_empty());
}

#[test]
fn registered_binding_is_consumed_by_first_publish_and_modify_resumes_same_code_session() {
    let (dir, lib, mut req) = setup();
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let source_directory = "/home/user/.lingxi/.apps/notes-1234abcd";
    let (source_id, request_id) = registered_source(&lib, source_directory, identity);
    let binding = authoring(source_directory);
    let session_id = binding.authoring_session_id.clone().unwrap();
    lib.register_native_authoring(binding.clone(), &source_id, &request_id, true, |_| {
        Ok(identity)
    })
    .unwrap();
    req.authoring = None;

    let first = lib
        .publish_with_native_authoring(
            req.clone(),
            source_directory,
            &session_id,
            None,
            None,
            true,
            |_| Ok(identity),
        )
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
        3
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
    assert_eq!(
        lib.register_native_authoring(binding.clone(), &source_id, &request_id, true, |_| Ok(
            identity
        ))
        .unwrap(),
        binding
    );

    req.app_id = Some(first.record.app_id.clone());
    req.expected_revision = 1;
    assert!(lib.publish(req.clone()).is_err());
    let modified = lib
        .publish_with_native_authoring(
            req,
            source_directory,
            &session_id,
            None,
            Some(&source_id),
            false,
            |_| Ok(identity),
        )
        .unwrap();
    assert_eq!(modified.record.authoring, first.record.authoring);
}

#[test]
fn failed_first_publish_keeps_registered_authoring_for_retry() {
    let (dir, lib, mut req) = setup();
    let source_directory = "/home/user/.lingxi/.apps/retry-1234abcd";
    let identity = rooted_fs::root_identity(dir.path()).unwrap();
    let (source_id, request_id) = registered_source(&lib, source_directory, identity);
    let binding = authoring(source_directory);
    let session_id = binding.authoring_session_id.clone().unwrap();
    lib.register_native_authoring(binding, &source_id, &request_id, true, |_| Ok(identity))
        .unwrap();
    req.authoring = None;
    let manifest_path = Path::new(&req.source_path).join("lingxi-app.json");
    let valid_manifest = fs::read(&manifest_path).unwrap();
    fs::write(&manifest_path, "{\"schema\":99}").unwrap();
    assert!(lib
        .publish_with_native_authoring(
            req.clone(),
            source_directory,
            &session_id,
            None,
            None,
            true,
            |_| Ok(identity)
        )
        .is_err());

    fs::write(&manifest_path, valid_manifest).unwrap();
    let retried = lib
        .publish_with_native_authoring(req, source_directory, &session_id, None, None, true, |_| {
            Ok(identity)
        })
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
