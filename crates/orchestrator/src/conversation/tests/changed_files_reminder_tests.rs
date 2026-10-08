use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, PermissionGate,
    StaticMemoryProvider,
};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tool_api::registry::ToolRegistry;
use tool_api::Tool;

fn orch() -> ConversationOrchestrator {
    let cwd = PathBuf::from("/work/repo");
    let policy = permission::PermissionPolicy::new(permission::PermissionMode::Default)
        .with_roots(permission::FsRoots {
            cwd: cwd.clone(),
            home: Some(cwd.join("home")),
            lingxi_home: cwd.join(branding::DOT_DIR),
        });
    orch_with_gate(
        cwd,
        Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(policy),
            Arc::new(NoOpPermissionGate),
        )),
    )
}

fn orch_with_gate(
    cwd: PathBuf,
    gate: Arc<dyn crate::test_support::PermissionGate>,
) -> ConversationOrchestrator {
    let mut orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        gate,
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        cwd,
    );
    let mut read_context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![PathBuf::from("/")],
    );
    read_context.read_file_state = orchestrator.prompt_runtime.read_state_map.clone();
    Arc::get_mut(&mut orchestrator.tools)
        .expect("test fixture exclusively owns its tool registry")
        .register_builtin(Arc::new(tool_file::FileReadTool::new(read_context)));
    orchestrator
}

fn policy_gate(cwd: &std::path::Path) -> Arc<dyn crate::test_support::PermissionGate> {
    let policy = permission::PermissionPolicy::new(permission::PermissionMode::Default).with_roots(
        permission::FsRoots {
            cwd: cwd.to_path_buf(),
            home: Some(cwd.join("home")),
            lingxi_home: cwd.join(branding::DOT_DIR),
        },
    );
    Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(policy),
        Arc::new(NoOpPermissionGate),
    ))
}

fn seed(
    orch: &ConversationOrchestrator,
    path: &std::path::Path,
    content: &str,
    mtime_ms: i64,
    entry: tool_api::read_file_state::ReadFileEntry,
) {
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        path.to_path_buf(),
        tool_api::read_file_state::ReadFileEntry {
            content: content.to_string(),
            mtime_ms,
            ..entry
        },
        true,
        Some(path.to_path_buf()),
    );
}

fn full_read(content: &str) -> tool_api::read_file_state::ReadFileEntry {
    tool_api::read_file_state::ReadFileEntry {
        content: content.to_string(),
        mtime_ms: 0,
        offset: None,
        limit: None,
        from_read: true,
        seeded_from_context: false,
        is_partial_view: false,
    }
}

#[tokio::test]
async fn a_file_changed_on_disk_emits_one_wrapped_reminder_and_does_not_repeat() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let orch = orch();
    // The model read the OLD bytes; the file has since been rewritten.
    seed(&orch, &path, "a\nOLD\nc\n", 0, full_read("a\nOLD\nc\n"));

    let msgs = orch.changed_files_reminder_messages(None).await;
    assert_eq!(msgs.len(), 1, "exactly one changed file");
    let text = msgs[0].text_content();
    assert!(text.starts_with("<system-reminder>\n"), "got: {text}");
    assert!(text.ends_with("\n</system-reminder>"), "got: {text}");
    assert!(
        text.contains("changed on disk since you last read it."),
        "2.1.238 copy expected; got: {text}"
    );
    assert!(
        !text.contains("either by the user or by a linter"),
        "the 2.1.220 wording must be gone; got: {text}"
    );
    assert!(
        text.contains("Here are the relevant changes (shown with line numbers):\n1\ta\n2\tb\n3\tc"),
        "numbered diff expected; got: {text}"
    );

    // The registered production Read tool refreshed the shared entry itself.
    let key = path.clone();
    {
        let state = orch.prompt_runtime.read_state_map.lock().unwrap();
        let refreshed = state.peek(&key).expect("actual Read updates shared state");
        assert_eq!(refreshed.content, "a\nb\nc\n");
        assert_eq!(
            state.changed_file_candidate_keys(),
            vec![key.clone()],
            "there is one actual Read entry and no reminder-owned duplicate"
        );
        assert!(state
            .requested_path_groups(&key)
            .iter()
            .any(|group| group.contains(&path)), "the selected source route stays captured");
    }
    // The real Read side effect refreshes mtime ⇒ silent next turn.
    assert!(orch.changed_files_reminder_messages(None).await.is_empty());
}

#[tokio::test]
async fn registered_read_suppresses_token_truncated_text_and_refreshes_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("token-cap.rs");
    let fresh = "x".repeat(100_004); // 25,001 rough tokens at the current 4 bytes/token ratio.
    std::fs::write(&path, &fresh).unwrap();
    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    seed(&orch, &path, "old text\n", 0, full_read("old text\n"));

    assert!(orch.changed_files_reminder_messages(None).await.is_empty());
    let key = path.clone();
    let state = orch.prompt_runtime.read_state_map.lock().unwrap();
    let entry = state.peek(&key).expect("actual Read updates token-capped state");
    assert!(entry.is_partial_view);
    assert!(entry.content.len() < fresh.len());
}

#[tokio::test]
async fn registered_read_suppresses_notebook_result_after_native_read_side_effect() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("changed.ipynb");
    let notebook = serde_json::json!({
        "cells": [{
            "cell_type": "code",
            "source": ["2+2"],
            "outputs": [],
            "metadata": {},
            "execution_count": null
        }],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    });
    std::fs::write(&path, serde_json::to_vec(&notebook).unwrap()).unwrap();
    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    seed(&orch, &path, "old notebook content", 0, full_read("old notebook content"));

    assert!(orch.changed_files_reminder_messages(None).await.is_empty());
    let key = path.clone();
    let state = orch.prompt_runtime.read_state_map.lock().unwrap();
    let entry = state.peek(&key).expect("Notebook Read refreshes shared state");
    assert_ne!(entry.content, "old notebook content");
    assert!(entry.content.contains("2+2"));
}

#[tokio::test]
async fn cancelled_actual_read_aborts_before_state_refresh_and_reminder_output() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cancelled.rs");
    std::fs::write(&path, "new content\n").unwrap();
    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    seed(&orch, &path, "old content\n", 0, full_read("old content\n"));
    let cancel = lingxi_core::host::CancellationToken::new();
    cancel.cancel();

    let read = orch.tools.find_registered("Read").expect("registered built-in Read");
    let mut call_context = crate::turn_loop::streaming_tool_context_base(&orch, vec![]).await;
    call_context.cancel = Some(cancel.clone());
    let (progress_tx, _progress_rx) = tool_api::progress_channel();
    assert!(matches!(
        read.call(
            serde_json::json!({"file_path": path.to_string_lossy()}),
            call_context,
            progress_tx,
        )
        .await,
        Err(tool_api::ToolError::Aborted)
    ));
    assert!(orch.changed_files_reminder_messages(Some(&cancel)).await.is_empty());
    let state = orch.prompt_runtime.read_state_map.lock().unwrap();
    assert_eq!(state.peek(&path).unwrap().content, "old content\n");
}

#[tokio::test]
async fn actual_read_image_result_is_carried_typed_but_default_native_renderer_is_empty() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pic.png");
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
        0x00, 0xb5, 0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78,
        0xda, 0x63, 0xfc, 0xff, 0x1f, 0x00, 0x03, 0x03, 0x02, 0x00, 0xef, 0x9a, 0x49, 0xf5,
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&path, PNG_1X1).unwrap();
    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    seed(&orch, &path, "old image marker", 0, full_read("old image marker"));
    let read = orch.tools.find_registered("Read").expect("registered built-in Read");
    let call_context = crate::turn_loop::streaming_tool_context_base(&orch, vec![]).await;
    let (progress_tx, _progress_rx) = tool_api::progress_channel();
    let result = read
        .call(
            serde_json::json!({"file_path": path.to_string_lossy()}),
            call_context,
            progress_tx,
        )
        .await;
    match result {
        Ok(result) => {
            assert_eq!(result.data["type"], "image");
            let changed = crate::prompt::changed_files::ChangedFile::from_read_result(
                path.to_string_lossy().into_owned(),
                &result.data,
            )
            .expect("actual Read image result remains typed");
            assert!(changed.image.is_some());
            assert!(crate::prompt::changed_files::render_changed_file_message(&changed, "Read")
                .is_none());
            // The current 2.1.291 default renderer maps this variant to [].
            assert!(orch.changed_files_reminder_messages(None).await.is_empty());
        }
        Err(error) => {
            // In builds without the image-read feature, current Read keeps its
            // native binary refusal and NTr has no image result to render.
            assert!(error.to_string().contains("binary"), "got: {error}");
            assert!(orch.changed_files_reminder_messages(None).await.is_empty());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn live_read_deny_added_for_original_symlink_alias_suppresses_changed_content() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let canonical_dir = dir.path().join("secrets");
    std::fs::create_dir(&canonical_dir).unwrap();
    let target_path = canonical_dir.join("a.rs");
    std::fs::write(&target_path, "new secret\n").unwrap();
    let canonical_path = tokio::fs::canonicalize(&target_path).await.unwrap();
    let alias = dir.path().join("open");
    symlink(&canonical_dir, &alias).unwrap();
    let requested_path = alias.join("a.rs");

    let policy = Arc::new(
        permission::PermissionPolicy::new(permission::PermissionMode::Default).with_roots(
            permission::FsRoots {
                cwd: dir.path().to_path_buf(),
                home: Some(dir.path().join("home")),
                lingxi_home: dir.path().join(branding::DOT_DIR),
            },
        ),
    );
    let gate = Arc::new(permission::PolicyPermissionGate::new(
        policy,
        Arc::new(NoOpPermissionGate),
    ));
    let orch = orch_with_gate(dir.path().to_path_buf(), gate.clone());
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        canonical_path.clone(),
        full_read("old secret\n"),
        true,
        Some(requested_path.clone()),
    );
    // This fixture refreshes the SAME observed alias route. A generic
    // model-context insertion at `canonical_path` would be a second current
    // Native source route, so keep that positive case in its dedicated test.
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        canonical_path.clone(),
        full_read("old secret\n"),
        true,
        Some(requested_path.clone()),
    );

    // The active rule changes after the content was first read. It names the
    // lexical alias, while the cache itself is keyed by the canonical target.
    gate.apply_permission_update(&serde_json::json!({
        "type": "addRules",
        "rules": [{"toolName": "Read", "ruleContent": "./open/**"}],
        "behavior": "deny",
        "destination": "session"
    }));

    assert!(
        orch.changed_files_reminder_messages(None).await.is_empty(),
        "a newly active deny on the original symlink spelling suppresses the diff"
    );
    let guard = orch.prompt_runtime.read_state_map.lock().unwrap();
    let entry = guard.peek(&canonical_path).expect("read entry retained");
    assert_eq!(
        entry.content, "old secret\n",
        "denied bytes are not refreshed"
    );
    assert_eq!(
        guard.requested_path_groups(&canonical_path),
        vec![vec![requested_path]],
        "the original Read spelling survives the denied reminder"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn live_deny_on_any_spelling_of_a_translated_route_suppresses_changed_content() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let target_dir = dir.path().join("secrets");
    std::fs::create_dir(&target_dir).unwrap();
    let target_path = target_dir.join("a.rs");
    std::fs::write(&target_path, "new secret\n").unwrap();
    let canonical_path = tokio::fs::canonicalize(&target_path).await.unwrap();
    let model_alias = dir.path().join("guest");
    let host_alias = dir.path().join("host");
    symlink(&target_dir, &model_alias).unwrap();
    symlink(&target_dir, &host_alias).unwrap();
    let model_path = model_alias.join("a.rs");
    let translated_path = host_alias.join("a.rs");

    let policy = Arc::new(
        permission::PermissionPolicy::new(permission::PermissionMode::Default).with_roots(
            permission::FsRoots {
                cwd: dir.path().to_path_buf(),
                home: Some(dir.path().join("home")),
                lingxi_home: dir.path().join(branding::DOT_DIR),
            },
        ),
    );
    let gate = Arc::new(permission::PolicyPermissionGate::new(
        policy,
        Arc::new(NoOpPermissionGate),
    ));
    let orch = orch_with_gate(dir.path().to_path_buf(), gate.clone());
    tool_api::read_file_state::set_with_requested_aliases(
        &orch.prompt_runtime.read_state_map,
        canonical_path.clone(),
        full_read("old secret\n"),
        true,
        vec![model_path, translated_path.clone()],
    );

    gate.apply_permission_update(&serde_json::json!({
        "type": "addRules",
        "rules": [{"toolName": "Read", "ruleContent": "./host/**"}],
        "behavior": "deny",
        "destination": "session"
    }));

    assert!(
        orch.changed_files_reminder_messages(None).await.is_empty(),
        "a deny on the translated host spelling blocks the whole model route"
    );
    assert_eq!(
        tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &canonical_path)
            .unwrap()
            .content,
        "old secret\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn changed_file_reminder_does_not_fall_back_when_original_symlink_is_gone() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let target_dir = dir.path().join("target");
    std::fs::create_dir(&target_dir).unwrap();
    let target_path = target_dir.join("a.rs");
    std::fs::write(&target_path, "new secret\n").unwrap();
    let canonical_path = tokio::fs::canonicalize(&target_path).await.unwrap();
    let alias = dir.path().join("open");
    symlink(&target_dir, &alias).unwrap();
    let requested_path = alias.join("a.rs");

    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        canonical_path.clone(),
        full_read("old secret\n"),
        true,
        Some(requested_path),
    );
    std::fs::remove_file(&alias).unwrap();

    assert!(
        orch.changed_files_reminder_messages(None).await.is_empty(),
        "a vanished model route cannot authorize reading the canonical cache key"
    );
    assert_eq!(
        tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &canonical_path)
            .unwrap()
            .content,
        "old secret\n",
        "the stale route is not refreshed from the target directly"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reminder_refresh_does_not_create_a_canonical_route_after_alias_disappears() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let target_dir = dir.path().join("target");
    std::fs::create_dir(&target_dir).unwrap();
    let target_path = target_dir.join("a.rs");
    std::fs::write(&target_path, "first changed version\n").unwrap();
    let alias = dir.path().join("open");
    symlink(&target_dir, &alias).unwrap();
    let requested_path = alias.join("a.rs");
    let cache_key = requested_path.clone();
    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        cache_key.clone(),
        full_read("old model version\n"),
        true,
        Some(requested_path.clone()),
    );

    let first = orch.changed_files_reminder_messages(None).await;
    assert_eq!(first.len(), 1, "the live alias route reaches the changed target");
    assert_eq!(
        tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &cache_key)
            .unwrap()
            .content,
        "first changed version\n",
        "the successful reminder refresh stores the bytes it read"
    );
    assert_eq!(
        orch.prompt_runtime
            .read_state_map
            .lock()
            .unwrap()
            .requested_path_groups(&cache_key),
        vec![vec![requested_path]],
        "refresh keeps its selected route and does not append the canonical cache key"
    );

    // Ensure the second on-disk rewrite has an mtime newer than the first
    // reminder's refresh on filesystems with coarse timestamp resolution.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    std::fs::remove_file(&alias).unwrap();
    std::fs::write(&target_path, "second target-only version\n").unwrap();
    assert!(
        orch.changed_files_reminder_messages(None).await.is_empty(),
        "after the model's original route disappears, the canonical target is not a fallback"
    );
    let canonical_target = std::fs::canonicalize(&target_path).unwrap();
    let state = orch.prompt_runtime.read_state_map.lock().unwrap();
    assert!(
        state.peek(&cache_key).is_none(),
        "the removed symlink route is dropped instead of replaced with a target route"
    );
    assert!(
        !state.changed_file_candidate_keys().contains(&canonical_target),
        "the reminder must not invent the canonical target as a new source route"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn absolute_route_parent_above_root_matches_the_clamped_target() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("parent-route.rs");
    std::fs::write(&target, "new bytes\n").unwrap();
    let cache_key = tokio::fs::canonicalize(&target).await.unwrap();
    let relative_to_root = target.strip_prefix("/").unwrap();
    let requested = std::path::Path::new("/../").join(relative_to_root);
    let gate = policy_gate(dir.path());
    gate.apply_permission_updates(&[serde_json::json!({
        "type": "addRules",
        "rules": [{"toolName":"Read", "ruleContent":"./unrelated/**"}],
        "behavior": "deny",
        "destination": "session"
    })]);
    let orch = orch_with_gate(dir.path().to_path_buf(), gate);
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        cache_key,
        full_read("old bytes\n"),
        true,
        Some(requested),
    );

    assert_eq!(
        orch.changed_files_reminder_messages(None).await.len(),
        1,
        "Native path.resolve clamps leading .. at the filesystem root before the route walk"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn changed_file_reminder_uses_first_permitted_alias_in_observation_order() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let canonical_dir = dir.path().join("secrets");
    std::fs::create_dir(&canonical_dir).unwrap();
    let target_path = canonical_dir.join("a.rs");
    std::fs::write(&target_path, "new secret\n").unwrap();
    let canonical_path = tokio::fs::canonicalize(&target_path).await.unwrap();
    let denied_alias = dir.path().join("closed");
    let permitted_alias = dir.path().join("open");
    symlink(&canonical_dir, &denied_alias).unwrap();
    symlink(&canonical_dir, &permitted_alias).unwrap();
    let denied_path = denied_alias.join("a.rs");
    let permitted_path = permitted_alias.join("a.rs");

    let policy = Arc::new(
        permission::PermissionPolicy::new(permission::PermissionMode::Default).with_roots(
            permission::FsRoots {
                cwd: dir.path().to_path_buf(),
                home: Some(dir.path().join("home")),
                lingxi_home: dir.path().join(branding::DOT_DIR),
            },
        ),
    );
    let gate = Arc::new(permission::PolicyPermissionGate::new(
        policy,
        Arc::new(NoOpPermissionGate),
    ));
    let orch = orch_with_gate(dir.path().to_path_buf(), gate.clone());
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        canonical_path.clone(),
        full_read("old secret\n"),
        true,
        Some(denied_path.clone()),
    );
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        canonical_path.clone(),
        full_read("old secret\n"),
        true,
        Some(permitted_path.clone()),
    );

    gate.apply_permission_update(&serde_json::json!({
        "type": "addRules",
        "rules": [{"toolName": "Read", "ruleContent": "./closed/**"}],
        "behavior": "deny",
        "destination": "session"
    }));

    let reminders = orch.changed_files_reminder_messages(None).await;
    assert_eq!(reminders.len(), 1);
    let text = reminders[0].text_content();
    assert!(
        text.contains(&permitted_path.to_string_lossy().to_string()),
        "{text}"
    );
    assert!(
        !text.contains(&denied_path.to_string_lossy().to_string()),
        "{text}"
    );
    assert_eq!(
        orch.prompt_runtime
            .read_state_map
            .lock()
            .unwrap()
            .requested_path_groups(&canonical_path),
        vec![vec![denied_path], vec![permitted_path]],
        "reminder refresh keeps all original routes in insertion order"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_changed_file_40_hop_sge_limit_suppresses_reminder_separately_from_direct_read() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let real_dir = dir.path().join("real");
    let route_dir = dir.path().join("route");
    std::fs::create_dir(&real_dir).unwrap();
    std::fs::create_dir(&route_dir).unwrap();
    let target_path = real_dir.join("a.rs");
    std::fs::write(&target_path, "new secret\n").unwrap();
    let canonical_path = tokio::fs::canonicalize(&target_path).await.unwrap();

    for index in (0..41).rev() {
        let link = route_dir.join(format!("dir-{index:03}"));
        let target = if index + 1 == 41 {
            PathBuf::from("../real")
        } else {
            PathBuf::from(format!("dir-{:03}", index + 1))
        };
        symlink(target, link).unwrap();
    }
    let requested_path = route_dir.join("dir-000/a.rs");

    let policy = Arc::new(
        permission::PermissionPolicy::new(permission::PermissionMode::Default).with_roots(
            permission::FsRoots {
                cwd: dir.path().to_path_buf(),
                home: Some(dir.path().join("home")),
                lingxi_home: dir.path().join(branding::DOT_DIR),
            },
        ),
    );
    let gate = Arc::new(permission::PolicyPermissionGate::new(
        policy,
        Arc::new(NoOpPermissionGate),
    ));
    let orch = orch_with_gate(dir.path().to_path_buf(), gate.clone());
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        canonical_path.clone(),
        full_read("old secret\n"),
        true,
        Some(requested_path),
    );
    // The live Read deny activates Native's reminder preflight, but this rule
    // does not match the route. Suppression therefore exercises Sge's 40-hop
    // limit rather than the direct permission resolver's 64-hop partial forms.
    gate.apply_permission_update(&serde_json::json!({
        "type": "addRules",
        "rules": [{"toolName": "Read", "ruleContent": "./unrelated/**"}],
        "behavior": "deny",
        "destination": "session"
    }));

    assert!(orch.changed_files_reminder_messages(None).await.is_empty());
    assert_eq!(
        tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &canonical_path)
            .unwrap()
            .content,
        "old secret\n",
        "the unresolved reminder route does not refresh the canonical I/O cache"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_sy_gate_uses_restricted_and_outside_block_and_checks_allowed_roots() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let project = dir.path().join("project");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let inside_path = project.join("inside.rs");
    let outside_path = outside.join("outside.rs");
    std::fs::write(&inside_path, "new inside\n").unwrap();
    std::fs::write(&outside_path, "new outside\n").unwrap();
    let inside_key = tokio::fs::canonicalize(&inside_path).await.unwrap();
    let outside_key = tokio::fs::canonicalize(&outside_path).await.unwrap();
    let alias = project.join("alias.rs");
    symlink(&inside_path, &alias).unwrap();

    for restricted in [true, false] {
        let policy = permission::PermissionPolicy::new(permission::PermissionMode::Default)
            .with_roots(permission::FsRoots {
                cwd: project.clone(),
                home: Some(dir.path().join("home")),
                lingxi_home: dir.path().join(branding::DOT_DIR),
            })
            .with_restricted(restricted)
            .with_block_reads_outside_working_directories(!restricted);
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(policy),
            Arc::new(NoOpPermissionGate),
        ));
        let orch = orch_with_gate(project.clone(), gate.clone());
        tool_api::read_file_state::set_with_requested_path(
            &orch.prompt_runtime.read_state_map,
            inside_key.clone(),
            full_read("old inside\n"),
            true,
            Some(alias.clone()),
        );
        tool_api::read_file_state::set_with_requested_path(
            &orch.prompt_runtime.read_state_map,
            outside_key.clone(),
            full_read("old outside\n"),
            true,
            Some(outside_path.clone()),
        );

        let facts = gate.read_path_policy_snapshot().facts();
        assert!(!facts.read_deny_rules_active);
        assert_eq!(facts.restricted, restricted);
        assert_eq!(facts.block_reads_outside_working_directories, !restricted);
        assert!(facts.requires_path_recheck());

        let reminders = orch.changed_files_reminder_messages(None).await;
        assert_eq!(reminders.len(), 1, "only the in-root route is admitted");
        let text = reminders[0].text_content();
        assert!(text.contains("alias.rs"), "{text}");
        assert!(!text.contains("outside.rs"), "{text}");
        assert_eq!(
            tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &outside_key)
                .unwrap()
                .content,
            "old outside\n",
            "outside-held bytes do not refresh the state cache"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn native_reminder_route_set_accepts_31_leaf_links_and_rejects_32() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let base = dir.path().join("routes");
    std::fs::create_dir(&base).unwrap();
    let roots = permission::FsRoots {
        cwd: dir.path().to_path_buf(),
        home: Some(dir.path().join("home")),
        lingxi_home: dir.path().join(branding::DOT_DIR),
    };
    let policy = permission::PermissionPolicy::new(permission::PermissionMode::Default)
        .with_roots(roots);
    let gate = Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(policy),
        Arc::new(NoOpPermissionGate),
    ));
    let orch = orch_with_gate(dir.path().to_path_buf(), gate.clone());
    let mut paths = Vec::new();
    for count in [31, 32] {
        let route_dir = base.join(format!("route-{count}"));
        std::fs::create_dir(&route_dir).unwrap();
        let target = route_dir.join("target.rs");
        std::fs::write(&target, format!("new route {count}\n")).unwrap();
        for index in (0..count).rev() {
            let link = route_dir.join(format!("link-{index:03}"));
            let next = if index + 1 == count {
                PathBuf::from("target.rs")
            } else {
                PathBuf::from(format!("link-{:03}", index + 1))
            };
            symlink(next, &link).unwrap();
        }
        let requested = route_dir.join("link-000");
        let key = tokio::fs::canonicalize(&target).await.unwrap();
        tool_api::read_file_state::set_with_requested_path(
            &orch.prompt_runtime.read_state_map,
            key,
            full_read(&format!("old route {count}\n")),
            true,
            Some(requested.clone()),
        );
        paths.push((count, requested));
    }
    gate.apply_permission_update(&serde_json::json!({
        "type": "addRules",
        "rules": [{"toolName": "Read", "ruleContent": "./unrelated/**"}],
        "behavior": "deny",
        "destination": "session"
    }));

    let reminders = orch.changed_files_reminder_messages(None).await;
    assert_eq!(reminders.len(), 1);
    let text = reminders[0].text_content();
    assert!(text.contains(&paths.iter().find(|(count, _)| *count == 31).unwrap().1.to_string_lossy().to_string()), "{text}");
    assert!(!text.contains(&paths.iter().find(|(count, _)| *count == 32).unwrap().1.to_string_lossy().to_string()), "{text}");
}

#[tokio::test]
async fn cancelled_changed_file_route_resolution_has_no_cache_side_effect() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "new\n").unwrap();
    let key = tokio::fs::canonicalize(&path).await.unwrap();
    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        key.clone(),
        full_read("old\n"),
        true,
        Some(path),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();

    assert!(orch.changed_files_reminder_messages(Some(&cancel)).await.is_empty());
    assert_eq!(
        tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &key)
            .unwrap()
            .content,
        "old\n"
    );
}

#[tokio::test]
async fn prepare_turn_step_forwards_cancellation_into_changed_file_producers() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "new\n").unwrap();
    let key = tokio::fs::canonicalize(&path).await.unwrap();
    let orch = orch_with_gate(dir.path().to_path_buf(), policy_gate(dir.path()));
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        key.clone(),
        full_read("old\n"),
        true,
        Some(path),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();

    let prepared = orch
        .prepare_turn_step(
            crate::conversation::ModelCallPath::Streaming,
            None,
            true,
            true,
            Some(&cancel),
        )
        .await
        .unwrap();

    assert!(
        prepared.snapshot.iter().all(|message| {
            !message
                .text_content()
                .contains("changed on disk since you last read it.")
        }),
        "the actual turn-preparation producer path must observe cancellation"
    );
    let refreshed = tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &key)
        .expect("cancelled reminder keeps its read-state entry");
    assert_eq!(refreshed.content, "old\n");
    assert_eq!(refreshed.mtime_ms, 0);
}

#[tokio::test]
async fn current_rootless_policy_unavailable_preserves_pre_seam_reminder_behavior() {
    use lingxi_core::host::permission_gate::{
        PermissionGate as _, ReadPathPolicyMatch,
        ReadPathPolicySnapshot as _,
    };

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "new\n").unwrap();
    let key = tokio::fs::canonicalize(&path).await.unwrap();
    let policy = permission::PermissionPolicy::new(permission::PermissionMode::Default)
        .with_restricted(true);
    let gate = Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(policy),
        Arc::new(NoOpPermissionGate),
    ));
    let orch = orch_with_gate(dir.path().to_path_buf(), gate.clone());
    tool_api::read_file_state::set_with_requested_path(
        &orch.prompt_runtime.read_state_map,
        key.clone(),
        full_read("old\n"),
        true,
        Some(path),
    );

    let snapshot = gate.read_path_policy_snapshot();
    assert_eq!(
        snapshot.check_path(&key).held_outside,
        ReadPathPolicyMatch::Unavailable,
        "rootless policy is not represented as a no-match"
    );
    let reminders = orch.changed_files_reminder_messages(None).await;
    assert_eq!(
        reminders.len(),
        1,
        "unavailable keeps the pre-seam changed-file behavior; this is not a Native hS parity claim"
    );
}

#[tokio::test]
async fn an_mtime_bump_with_identical_bytes_emits_nothing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "same\n").unwrap();
    let orch = orch();
    seed(&orch, &path, "same\n", 0, full_read("same\n"));
    assert!(
        orch.changed_files_reminder_messages(None).await.is_empty(),
        "`vNe` content compare suppresses the reminder"
    );
}

#[tokio::test]
async fn rendered_memory_seed_retains_prompt_route_and_can_emit_changed_file_notice() {
    use memory::lingxi_md::LingxiMdTier;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("LINGXI.md");
    std::fs::write(&path, "# changed on disk\n").unwrap();
    let orch = orch();
    let memory = crate::prompt::MemoryFile {
        parent: None,
        source_content: None,
        path: path.clone(),
        body: "# rendered memory\n".into(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: None,
        raw_content: "# rendered memory\n".into(),
        content_differs_from_disk: false,
    };

    orch.seed_memory_read_state(std::slice::from_ref(&memory)).await;
    let key = crate::turn_loop::normalize_lexically(&path);
    {
        let mut state = orch.prompt_runtime.read_state_map.lock().unwrap();
        let mut entry = state.peek(&key).unwrap();
        assert!(entry.seeded_from_context);
        // Make the mtime gate deterministic while preserving the producer's
        // route and provenance in the actual read-state slot.
        entry.mtime_ms = 0;
        state.set_with_requested_path(key.clone(), entry, false, None);
        assert_eq!(state.requested_path_groups(&key), vec![vec![path.clone()]]);
        assert!(state.changed_file_candidate_keys().contains(&key));
    }

    let reminders = orch.changed_files_reminder_messages(None).await;
    assert_eq!(reminders.len(), 1, "rendered memory was in Native readFileState");
    assert!(reminders[0].text_content().contains("changed on disk since you last read it."));
}

#[tokio::test]
async fn nested_memory_seed_retains_prompt_route_for_changed_file_checks() {
    use memory::lingxi_md::LingxiMdTier;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested/LINGXI.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "new nested memory\n").unwrap();
    let orch = orch();
    let memory = crate::prompt::MemoryFile {
        parent: None,
        source_content: None,
        path: path.clone(),
        body: "old nested memory\n".into(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: None,
        raw_content: "old nested memory\n".into(),
        content_differs_from_disk: false,
    };

    assert!(orch.seed_nested_memory_read_state(&memory).await);
    let key = crate::turn_loop::normalize_lexically(&path);
    {
        let mut state = orch.prompt_runtime.read_state_map.lock().unwrap();
        let mut entry = state.peek(&key).unwrap();
        assert!(entry.seeded_from_context);
        entry.mtime_ms = 0;
        state.set_with_requested_path(key.clone(), entry, false, None);
        assert_eq!(state.requested_path_groups(&key), vec![vec![path.clone()]]);
        assert!(state.changed_file_candidate_keys().contains(&key));
    }

    assert_eq!(orch.changed_files_reminder_messages(None).await.len(), 1);
}

#[tokio::test]
async fn unrendered_memory_seed_retains_native_route_for_changed_file_checks() {
    use memory::lingxi_md::LingxiMdTier;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.md");
    std::fs::write(&path, "before seed\n").unwrap();
    let orch = orch();
    let memory = crate::prompt::MemoryFile {
        parent: None,
        source_content: None,
        path: path.clone(),
        body: String::new(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: None,
        raw_content: String::new(),
        content_differs_from_disk: false,
    };

    orch.seed_memory_read_state(std::slice::from_ref(&memory)).await;
    let key = crate::turn_loop::normalize_lexically(&path);
    {
        let state = orch.prompt_runtime.read_state_map.lock().unwrap();
        let entry = state.peek(&key).unwrap();
        assert!(!entry.seeded_from_context);
        assert_eq!(
            state.requested_path_groups(&key),
            vec![vec![path.clone()]],
            "Native processes this current seed despite contentNotInModelContext"
        );
        assert!(state.changed_file_candidate_keys().contains(&key));
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    std::fs::write(&path, "changed after seed\n").unwrap();
    let reminders = orch.changed_files_reminder_messages(None).await;
    assert_eq!(reminders.len(), 1, "the live non-rendered seed is a Native candidate");
    assert!(reminders[0].text_content().contains("changed after seed"));
}

#[tokio::test]
async fn partial_reads_skip_but_frontmatter_memory_uses_raw_disk_content() {
    use memory::lingxi_md::LingxiMdTier;

    let dir = TempDir::new().unwrap();
    let orch = orch();

    let partial = dir.path().join("partial.rs");
    std::fs::write(&partial, "x\ny\n").unwrap();
    seed(
        &orch,
        &partial,
        "OLD\n",
        0,
        tool_api::read_file_state::ReadFileEntry {
            offset: Some(1),
            limit: Some(10),
            ..full_read("OLD\n")
        },
    );

    let memory_path = dir.path().join("LINGXI.md");
    let old_raw = "---\npaths: src/**\n---\nold body\n";
    std::fs::write(&memory_path, "---\npaths: src/**\n---\nnew body\n").unwrap();
    let memory = crate::prompt::MemoryFile {
        parent: None,
        source_content: None,
        path: memory_path.clone(),
        body: "old body\n".into(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: Some(vec!["src/**".into()]),
        raw_content: old_raw.into(),
        content_differs_from_disk: true,
    };
    orch.seed_memory_read_state(std::slice::from_ref(&memory)).await;
    let key = crate::turn_loop::normalize_lexically(&memory_path);
    {
        let mut state = orch.prompt_runtime.read_state_map.lock().unwrap();
        let mut entry = state.peek(&key).unwrap();
        assert!(entry.is_partial_view, "the marker is memory parse provenance");
        assert_eq!(entry.content, old_raw, "changed-file diff uses exact raw disk bytes");
        entry.mtime_ms = 0;
        state.set_with_requested_path(key, entry, false, None);
    }

    let reminders = orch.changed_files_reminder_messages(None).await;
    assert_eq!(reminders.len(), 1, "the partial Read entry stays filtered, but full raw memory remains eligible");
    assert!(reminders[0].text_content().contains("new body"));
}

#[tokio::test]
async fn a_vanished_file_drops_its_read_state_entry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("gone.rs");
    let orch = orch();
    seed(&orch, &path, "old\n", 0, full_read("old\n"));
    assert!(orch.changed_files_reminder_messages(None).await.is_empty());
    assert!(
        !orch
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&path),
        "`if(ur(c))e.readFileState.delete(s)` — the entry must be dropped"
    );
}

#[tokio::test]
async fn host_seed_records_its_current_route_and_can_emit_changed_file_notice() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("host.txt");
    std::fs::write(&path, "old host snapshot\n").unwrap();
    let orch = orch();
    let host_mtime_ms = std::fs::metadata(&path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    #[cfg(unix)]
    let requested = std::path::Path::new("/")
        .join("..")
        .join(path.strip_prefix("/").unwrap());
    #[cfg(not(unix))]
    let requested = path.clone();
    assert!(orch
        .seed_read_state_from_host(requested.to_str().unwrap(), host_mtime_ms)
        .await);
    // The host seed follows Native Ke(path.resolve): an above-root parent is
    // clamped, and the resulting absolute lexical route is BOTH cache key and
    // captured source. It does not resolve symlinks to realpath.
    let key = crate::turn_loop::normalize_lexically(&path);
    assert_eq!(key, path, "this fixture has no dot segments to normalize");
    {
        let mut state = orch.prompt_runtime.read_state_map.lock().unwrap();
        assert_eq!(state.requested_path_groups(&key), vec![vec![path.clone()]]);
        assert!(state.changed_file_candidate_keys().contains(&key));
        let mut entry = state.peek(&key).unwrap();
        entry.content = "old host snapshot\n".into();
        entry.mtime_ms = 0;
        state.set_with_requested_path(key.clone(), entry, false, None);
    }
    std::fs::write(&path, "new host snapshot\n").unwrap();

    let reminders = orch.changed_files_reminder_messages(None).await;
    assert_eq!(reminders.len(), 1, "Native NTr scans host-seeded read-state keys");
    assert!(reminders[0].text_content().contains("new host snapshot"));
}


#[tokio::test]
async fn public_model_context_setter_captures_its_route_at_insertion() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "new\n").unwrap();
    let orch = orch();
    tool_api::read_file_state::set_with_model_context(
        &orch.prompt_runtime.read_state_map,
        path.clone(),
        full_read("old\n"),
        true,
    );

    assert_eq!(
        orch.prompt_runtime
            .read_state_map
            .lock()
            .unwrap()
            .requested_path_groups(&path),
        vec![vec![path.clone()]],
        "the current model-visible setter captures its supplied path at production time"
    );
    assert_eq!(orch.changed_files_reminder_messages(None).await.len(), 1);
}

#[tokio::test]
async fn the_snippet_budget_blanks_later_files() {
    let dir = TempDir::new().unwrap();
    let orch = orch();

    let small = dir.path().join("small.rs");
    std::fs::write(&small, "new\n").unwrap();
    seed(&orch, &small, "old\n", 0, full_read("old\n"));

    let bulk: String = (0..4000).map(|i| format!("l{i}\n")).collect();
    for name in ["big1.rs", "big2.rs"] {
        let p = dir.path().join(name);
        std::fs::write(&p, &bulk).unwrap();
        seed(&orch, &p, "l0\n", 0, full_read("l0\n"));
    }

    let msgs = orch.changed_files_reminder_messages(None).await;
    assert_eq!(msgs.len(), 3, "three changed files");
    let last = msgs[2].text_content();
    assert!(
        last.contains(&small.to_string_lossy().to_string()),
        "the LRU-oldest entry is rendered last; got: {last}"
    );
    assert!(
        last.contains(
            "The diff is omitted here because other changed files this turn already filled \
the snippet budget; use Read if you need the current content."
        ),
        "the post-budget file must be blanked; got: {last}"
    );
    assert!(
        msgs[0]
            .text_content()
            .contains("Here are the relevant changes (shown with line numbers):"),
        "the first file keeps its snippet"
    );
}
