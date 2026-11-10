use super::*;
use crate::{BuiltinHookHandler, HookResponse};
use lingxi_core::host::{
    BackgroundTaskHandle, HttpError, HttpTransport, RuntimeError, RuntimeSpawner,
};
use lingxi_core::types::{HttpRequest, HttpResponse};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::RwLock;

struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}
struct UnusedRuntime;
#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _: &str,
        _: Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        panic!("worktree lifecycle must be awaited, even for a nonblocking definition")
    }
    async fn sleep(&self, _: Duration) {}
    async fn cancel(&self, _: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}
#[derive(Default)]
struct PlainManager {
    creates: AtomicUsize,
    removes: AtomicUsize,
    summaries: AtomicUsize,
}
#[async_trait]
impl WorktreeManager for PlainManager {
    async fn create_worktree(
        &self,
        _: &str,
        _: Option<&str>,
        _: &[PathBuf],
    ) -> Result<WorktreeHandle, WorktreeError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok(WorktreeHandle {
            path: "/plain/git-tree".into(),
            branch_name: "worktree-native".into(),
            base_commit: Some("base".into()),
        })
    }
    async fn remove_worktree(&self, _: &WorktreeHandle) -> Result<(), WorktreeError> {
        self.removes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        Ok(vec![])
    }
    async fn cleanup_stale(&self, _: Duration) -> Result<Vec<PathBuf>, WorktreeError> {
        Ok(vec![])
    }
    fn is_supported(&self) -> bool {
        false
    }
    async fn worktree_change_summary(
        &self,
        _: &WorktreeHandle,
    ) -> Result<Option<WorktreeChangeSummary>, WorktreeError> {
        self.summaries.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

struct FixedHook {
    name: String,
    result: HookResult,
    seen: Arc<Mutex<Vec<(HookEvent, HookContext)>>>,
}
#[async_trait]
impl BuiltinHookHandler for FixedHook {
    fn id(&self) -> &str {
        &self.name
    }
    async fn handle(&self, event: &HookEvent, context: &HookContext) -> HookResult {
        self.seen
            .lock()
            .unwrap()
            .push((event.clone(), context.clone()));
        self.result.clone()
    }
}
fn success(path: Option<&str>) -> HookResult {
    HookResult {
        outcome: HookOutcome::Success,
        stdout: String::new().into(),
        stderr: String::new().into(),
        exit_code: None,
        response: Some(HookResponse {
            worktree_path: path.map(Into::into),
            ..HookResponse::default()
        }),
    }
}
fn definition(name: &str, event: HookEventType, priority: i32) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: name.into(),
        events: vec![event],
        if_condition: None,
        executor: HookExecutor::Builtin {
            handler_id: name.into(),
        },
        source: HookSource::Session,
        blocking: false,
        timeout: None,
        priority,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
        on_failure: None,
    }
}
fn manager(
    base: &Path,
    specs: Vec<(HookDefinition, HookResult)>,
) -> (
    HookedWorktreeManager,
    Arc<PlainManager>,
    Arc<Mutex<Vec<(HookEvent, HookContext)>>>,
) {
    let mut registry = crate::HookRegistry::new();
    for (definition, _) in &specs {
        registry.register(definition.clone());
    }
    let mut executor = HookExecutorImpl::new(
        Arc::new(RwLock::new(registry)),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    );
    let seen = Arc::new(Mutex::new(vec![]));
    for (definition, result) in specs {
        executor.register_builtin(Arc::new(FixedHook {
            name: definition.name,
            result,
            seen: seen.clone(),
        }));
    }
    let plain = Arc::new(PlainManager::default());
    let cwd = base.to_path_buf();
    let adapter = HookedWorktreeManager::new(
        plain.clone(),
        Arc::new(executor),
        cwd.clone(),
        Arc::new(move || HookContext {
            cwd: cwd.clone(),
            transcript_path: "/isolated/session.jsonl".into(),
            ..HookContext::default()
        }),
    );
    (adapter, plain, seen)
}

#[tokio::test]
async fn no_create_hook_delegates_without_extra_lifecycle() {
    let (manager, plain, seen) = manager(Path::new("/repo"), vec![]);
    assert!(!manager.is_supported());
    let handle = manager.create_worktree("native", None, &[]).await.unwrap();
    assert!(!manager.is_hook_based(&handle));
    manager.remove_worktree(&handle).await.unwrap();
    assert_eq!(plain.creates.load(Ordering::SeqCst), 1);
    assert_eq!(plain.removes.load(Ordering::SeqCst), 1);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_hook_selects_before_git_and_relative_path_uses_original_cwd() {
    let root = tempfile::tempdir().unwrap();
    let selected = root.path().join("selected");
    std::fs::create_dir(&selected).unwrap();
    let (manager, plain, seen) = manager(
        root.path(),
        vec![(
            definition("create", HookEventType::WorktreeCreate, 1),
            success(Some("./nested/../selected")),
        )],
    );
    let handle = manager.create_worktree("native", None, &[]).await.unwrap();
    assert_eq!(handle.path, selected);
    assert!(handle.branch_name.is_empty());
    assert!(manager.is_hook_based(&handle));
    assert_eq!(plain.creates.load(Ordering::SeqCst), 0);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(
        matches!(&seen[0].0, HookEvent::WorktreeCreate { name, path, .. } if name == "native" && path.as_os_str().is_empty())
    );
    assert_eq!(seen[0].1.cwd, root.path());
}

#[tokio::test]
async fn first_successful_nonempty_hook_wins_even_if_another_failed() {
    let root = tempfile::tempdir().unwrap();
    let failure = HookResult {
        outcome: HookOutcome::Error,
        stderr: "failed earlier".into(),
        exit_code: Some(1),
        ..success(None)
    };
    let (manager, plain, seen) = manager(
        root.path(),
        vec![
            (
                definition("failed", HookEventType::WorktreeCreate, 30),
                failure,
            ),
            (
                definition("empty", HookEventType::WorktreeCreate, 20),
                success(Some(" \n")),
            ),
            (
                definition("first", HookEventType::WorktreeCreate, 10),
                success(Some("first")),
            ),
            (
                definition("later", HookEventType::WorktreeCreate, 0),
                success(Some("later")),
            ),
        ],
    );
    assert_eq!(
        manager
            .create_worktree("native", None, &[])
            .await
            .unwrap()
            .path,
        root.path().join("first")
    );
    assert_eq!(seen.lock().unwrap().len(), 4);
    assert_eq!(plain.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn configured_but_no_answer_and_untrusted_creation_fail() {
    let (manager, plain, _) = manager(
        Path::new("/repo"),
        vec![(
            definition("empty", HookEventType::WorktreeCreate, 1),
            success(None),
        )],
    );
    assert_eq!(
        manager
            .create_worktree("native", None, &[])
            .await
            .unwrap_err()
            .to_string(),
        NO_CREATE_PATH
    );
    let manager = manager.with_workspace_trust_provider(Arc::new(|| false));
    assert_eq!(
        manager
            .create_worktree("native", None, &[])
            .await
            .unwrap_err()
            .to_string(),
        CONFIGURED_NO_CREATE
    );
    assert_eq!(plain.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn configured_but_policy_disabled_is_an_error_instead_of_git_fallback() {
    let plain = Arc::new(PlainManager::default());
    let mut registry = crate::HookRegistry::new();
    registry.register(definition("empty", HookEventType::WorktreeCreate, 1));
    let hooks = Arc::new(
        HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_policy_disable_all_hooks(true),
    );
    let manager = HookedWorktreeManager::new(
        plain.clone(),
        hooks,
        "/repo".into(),
        Arc::new(HookContext::default),
    );
    assert_eq!(
        manager
            .create_worktree("native", None, &[])
            .await
            .unwrap_err()
            .to_string(),
        CONFIGURED_NO_CREATE
    );
    assert_eq!(plain.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hook_owned_removal_never_reaches_git_and_restored_ownership_survives() {
    let root = tempfile::tempdir().unwrap();
    let hook_path = root.path().join("owned");
    std::fs::create_dir(&hook_path).unwrap();
    let (manager, plain, seen) = manager(
        root.path(),
        vec![(
            definition("remove", HookEventType::WorktreeRemove, 1),
            success(None),
        )],
    );
    let handle = WorktreeHandle {
        path: hook_path.clone(),
        branch_name: String::new(),
        base_commit: None,
    };
    manager.restore_hook_worktree(&handle);
    manager.remove_worktree(&handle).await.unwrap();
    assert_eq!(plain.removes.load(Ordering::SeqCst), 0);
    assert!(
        hook_path.exists(),
        "the hook owns deletion; the wrapper never deletes folders"
    );
    assert!(
        matches!(&seen.lock().unwrap()[0].0, HookEvent::WorktreeRemove { path } if path == &hook_path)
    );
}

#[tokio::test]
async fn missing_or_failed_remove_hook_preserves_folder_and_reports_failure() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("owned");
    std::fs::create_dir(&path).unwrap();
    let handle = WorktreeHandle {
        path: path.clone(),
        branch_name: String::new(),
        base_commit: None,
    };
    let (missing, plain, _) = manager(root.path(), vec![]);
    missing.restore_hook_worktree(&handle);
    assert!(missing
        .remove_worktree(&handle)
        .await
        .unwrap_err()
        .to_string()
        .contains("No WorktreeRemove hook"));
    assert_eq!(plain.removes.load(Ordering::SeqCst), 0);
    let failure = HookResult {
        outcome: HookOutcome::Error,
        stderr: "\nremove denied\nsecond line".into(),
        exit_code: Some(7),
        ..success(None)
    };
    let (failed, plain, _) = manager(
        root.path(),
        vec![(
            definition("remove", HookEventType::WorktreeRemove, 1),
            failure,
        )],
    );
    failed.restore_hook_worktree(&handle);
    assert!(failed
        .remove_worktree(&handle)
        .await
        .unwrap_err()
        .to_string()
        .contains("exited 7 (“remove denied”)"));
    assert!(path.exists());
    assert_eq!(plain.removes.load(Ordering::SeqCst), 0);
    std::fs::remove_dir(&path).unwrap();
    failed.remove_worktree(&handle).await.unwrap();
}

#[test]
fn exact_path_projection_and_command_last_line_are_distinct() {
    let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#"{"hookSpecificOutput":{"hookEventName":"WorktreeCreate","worktreePath":"chosen\ud800"}}"#).unwrap();
    let response = crate::parse_response_projection(&source, "WorktreeCreate").unwrap();
    assert_eq!(
        response
            .worktree_path
            .as_ref()
            .unwrap()
            .utf16_code_units
            .last(),
        Some(&0xd800)
    );
    let result = HookResult {
        response: Some(response),
        stdout: "log\n\u{1b}[31m /command/path \u{1b}[0m\n\n".into(),
        ..success(None)
    };
    let mut hook = definition("command", HookEventType::WorktreeCreate, 1);
    hook.executor = HookExecutor::Command {
        command: "create".into(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
        shell: None,
    };
    assert_eq!(
        WorktreeHookResult::from_result(&hook, &result)
            .selected_path()
            .unwrap()
            .display,
        "/command/path"
    );
    hook.executor = HookExecutor::Builtin {
        handler_id: "callback".into(),
    };
    assert_eq!(
        WorktreeHookResult::from_result(&hook, &result)
            .selected_path()
            .unwrap()
            .utf16_code_units
            .last(),
        Some(&0xd800)
    );
    assert!(crate::parse_response_projection(&source, "PostToolUse").is_err());
}

#[test]
fn personal_classifier_does_not_turn_host_callbacks_into_personal_hooks() {
    let (manager, _, _) = manager(Path::new("/repo"), vec![]);
    let manager = manager
        .with_restrict_personal_config(true)
        .with_personal_hook_classifier(Arc::new(|_, _| true));
    let hook = definition("callback", HookEventType::WorktreeCreate, 1);
    let mut result = WorktreeHookResult::from_result(&hook, &success(Some("/owned")));
    assert!(!manager.held(&result));
    result.can_be_personal = true;
    assert!(manager.held(&result));
}

#[test]
fn absolute_dots_and_repository_symlink_ancestry_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        screen_hook_path(&root.path().join("a/../tree"), root.path())
            .unwrap_err()
            .to_string()
            .contains("dot segments")
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/tmp", root.path().join("redirect")).unwrap();
        assert!(
            screen_hook_path(&root.path().join("redirect/tree"), root.path())
                .unwrap_err()
                .to_string()
                .contains("is a symlink")
        );
    }
}

#[test]
fn native_322_fixture_first_success_and_last_line_preserve_utf16() {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    let fixture = Utf16JsonProjection::parse(include_str!(
        "../../tests/fixtures/worktree_hooks_2_1_322.json"
    ))
    .unwrap();
    let cases = fixture
        .at_pointer("/create")
        .unwrap()
        .array_entries()
        .unwrap()
        .1;
    for (case_index, case) in cases {
        let id = case.get_property("id").unwrap().as_str().unwrap();
        let entries = case
            .get_property("results")
            .unwrap()
            .array_entries()
            .unwrap()
            .1;
        let selected = entries
            .into_iter()
            .filter(|(_, result)| {
                result
                    .get_property("succeeded")
                    .and_then(|value| value.as_bool())
                    == Some(true)
                    && result
                        .get_property("answerHeld")
                        .and_then(|value| value.as_bool())
                        != Some(true)
            })
            .find_map(|(result_index, _)| {
                let output = ExactHookText::from_json_projection(
                    &fixture,
                    &format!("/create/{case_index}/results/{result_index}/output"),
                )
                .unwrap();
                let result = WorktreeHookResult {
                    hook_id: HookId::new(),
                    source: HookSource::Session,
                    command: "callback".into(),
                    succeeded: true,
                    output,
                    exit_code: None,
                    can_be_personal: false,
                };
                result.selected_path()
            });
        let expected = ExactHookText::from_json_projection(
            &fixture,
            &format!("/create/{case_index}/expected/worktreePath"),
        );
        match (selected, expected) {
            (Some(selected), Some(expected)) => {
                assert_eq!(
                    absolute_path(
                        Path::new(
                            if case
                                .get_property("remote")
                                .and_then(|value| value.as_bool())
                                == Some(true)
                            {
                                "/original/repo"
                            } else {
                                case.get_property("cwd")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("/original/repo")
                            }
                        ),
                        &selected.display,
                    ),
                    PathBuf::from(&expected.display),
                    "{id}"
                );
                if selected.utf16_code_units.last() == Some(&0xd800) {
                    assert_eq!(expected.utf16_code_units.last(), Some(&0xd800), "{id}");
                }
            }
            (None, None) => {}
            (selected, expected) => panic!("{id}: selected={selected:?}, expected={expected:?}"),
        }
    }
}

struct ReturnHookPath;
#[async_trait]
impl crate::HookMcpInvoker for ReturnHookPath {
    async fn invoke(&self, _: crate::HookMcpInvocation) -> crate::HookMcpInvocationResult {
        crate::HookMcpInvocationResult::Success {
            text_content: vec!["chosen-by-loader-hook".into()],
        }
    }
}

#[tokio::test]
async fn restricted_paths_use_live_loader_origins_and_hold_ambiguous_bundles() {
    for source in [
        HookSource::Plugin,
        HookSource::FrontMatter,
        HookSource::Skill,
    ] {
        let mut hook = definition("loader-hook", HookEventType::WorktreeCreate, 1);
        hook.source = source;
        hook.executor = HookExecutor::McpTool {
            server: "already-connected".into(),
            tool: "create".into(),
            input: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
        };
        let mut registry = crate::HookRegistry::new();
        registry.register(hook.clone());
        let registry = Arc::new(RwLock::new(registry));
        let executor = Arc::new(
            HookExecutorImpl::new(
                registry.clone(),
                Arc::new(UnusedHttp),
                Arc::new(UnusedRuntime),
            )
            .with_mcp_invoker(Arc::new(ReturnHookPath)),
        );
        let manager = HookedWorktreeManager::new(
            Arc::new(PlainManager::default()),
            executor,
            "/repo".into(),
            Arc::new(HookContext::default),
        )
        .with_personal_hook_registry(registry.clone())
        .with_restrict_personal_config(true);
        assert!(
            manager
                .create_worktree("native", None, &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("doesn't let your own hooks choose"),
            "{source:?}"
        );
        // The actual loader can establish a built-in/managed/project owner,
        // and the existing manager observes it without being reconstructed.
        registry
            .write()
            .await
            .set_personal_hook_origins(std::slice::from_ref(&hook), Some(false));
        assert_eq!(
            manager
                .create_worktree("native", None, &[])
                .await
                .unwrap()
                .path,
            Path::new("/repo/chosen-by-loader-hook"),
            "{source:?}"
        );
        registry
            .write()
            .await
            .set_personal_hook_origins(std::slice::from_ref(&hook), Some(true));
        assert!(
            manager.create_worktree("native", None, &[]).await.is_err(),
            "{source:?}"
        );
        let manager = manager.with_restrict_personal_config(false);
        assert!(
            manager.create_worktree("native", None, &[]).await.is_ok(),
            "ordinary {source:?}"
        );
    }
}

#[tokio::test]
async fn personal_settings_callbacks_remain_host_owned_in_restricted_sessions() {
    let mut hook = definition("callback", HookEventType::WorktreeCreate, 1);
    hook.source = HookSource::Settings(lingxi_core::types::SettingsScope::User);
    let (manager, plain, _) = manager(
        Path::new("/repo"),
        vec![(hook, success(Some("/callback-owned")))],
    );
    let manager = manager.with_restrict_personal_config(true);
    assert_eq!(
        manager
            .create_worktree("native", None, &[])
            .await
            .unwrap()
            .path,
        Path::new("/callback-owned")
    );
    assert_eq!(plain.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn relative_path_uses_live_local_cwd_and_explicit_remote_original_base() {
    let mut hook = definition("create", HookEventType::WorktreeCreate, 1);
    hook.source = HookSource::Session;
    let (mut adapter, _, _) = manager(
        Path::new("/original"),
        vec![(hook, success(Some("relative/tree")))],
    );
    adapter.context = Arc::new(|| HookContext {
        cwd: "/live/current".into(),
        ..HookContext::default()
    });
    assert_eq!(
        adapter
            .create_worktree("native", None, &[])
            .await
            .unwrap()
            .path,
        Path::new("/live/current/relative/tree")
    );
    let adapter = adapter.with_remote_original_base(true);
    assert_eq!(
        adapter
            .create_worktree("native", None, &[])
            .await
            .unwrap()
            .path,
        Path::new("/original/relative/tree")
    );
}

#[test]
fn native_outside_repl_diagnostics_use_raw_command_and_stderr_on_failed_commands() {
    let mut hook = definition("create", HookEventType::WorktreeCreate, 1);
    hook.executor = HookExecutor::Command {
        command: "create-worktree".into(),
        args: vec!["arg".into()],
        env: HashMap::new(),
        cwd: None,
        shell: None,
    };
    hook.status_message = Some("friendly spinner".into());
    let failed = HookResult {
        outcome: HookOutcome::Error,
        stdout: "not a successful path".into(),
        stderr: "".into(),
        exit_code: Some(1),
        response: None,
    };
    let result = WorktreeHookResult::from_result(&hook, &failed);
    assert_eq!(result.command.display, "create-worktree arg");
    assert!(result.output.is_empty());
}

#[test]
fn native_322_fixture_error_text_uses_full_shipped_display_closure() {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    let fixture = Utf16JsonProjection::parse(include_str!(
        "../../tests/fixtures/worktree_hooks_2_1_322.json"
    ))
    .unwrap();
    for (case_index, case) in fixture
        .at_pointer("/create")
        .unwrap()
        .array_entries()
        .unwrap()
        .1
    {
        let Some(expected) = case
            .get_property("expected")
            .and_then(|value| value.get_property("error"))
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        let mut failures = Vec::new();
        for (result_index, result) in case
            .get_property("results")
            .unwrap()
            .array_entries()
            .unwrap()
            .1
        {
            if result
                .get_property("succeeded")
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                continue;
            }
            let output = ExactHookText::from_json_projection(
                &fixture,
                &format!("/create/{case_index}/results/{result_index}/output"),
            )
            .unwrap();
            let formatted = native_diagnostic(&output, "create").unwrap();
            failures.push(format!(
                "{}: {}",
                result
                    .get_property("command")
                    .and_then(|value| value.as_str())
                    .unwrap_or("callback"),
                if formatted.is_empty() {
                    "no output"
                } else {
                    &formatted.display
                }
            ));
        }
        if !failures.is_empty() {
            assert_eq!(
                format!("WorktreeCreate hook failed: {}", failures.join("; ")),
                expected
            );
        }
    }
    for (case_index, case) in fixture
        .at_pointer("/remove")
        .unwrap()
        .array_entries()
        .unwrap()
        .1
    {
        if case.get_property("gone").and_then(|value| value.as_bool()) == Some(true) {
            continue;
        }
        let Some(expected) = case
            .get_property("expected")
            .and_then(|value| value.get_property("summary"))
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        let Some((result_index, result)) = case
            .get_property("results")
            .unwrap()
            .array_entries()
            .unwrap()
            .1
            .into_iter()
            .find(|(_, result)| {
                result
                    .get_property("succeeded")
                    .and_then(|value| value.as_bool())
                    != Some(true)
            })
        else {
            continue;
        };
        let output = ExactHookText::from_json_projection(
            &fixture,
            &format!("/remove/{case_index}/results/{result_index}/output"),
        )
        .unwrap();
        let formatted = native_diagnostic(&output, "remove").unwrap();
        let detail = if formatted.is_empty() {
            String::new()
        } else {
            format!(" ({})", formatted.display)
        };
        let actual = if let Some(code) = result
            .get_property("exitCode")
            .and_then(|value| value.as_number())
        {
            format!("exited {code}{detail}")
        } else {
            format!(
                "failed{}",
                if detail.is_empty() {
                    " with no output"
                } else {
                    &detail
                }
            )
        };
        assert_eq!(actual, expected);
    }
}

#[test]
#[cfg(unix)]
fn nested_launch_cwd_still_screens_symlinks_below_checkout_root() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let nested_cwd = root.path().join("src/deep");
    std::fs::create_dir_all(&nested_cwd).unwrap();
    std::os::unix::fs::symlink("/tmp", root.path().join("sibling-redirect")).unwrap();
    let selected = root.path().join("sibling-redirect/selected");
    assert!(screen_hook_path(&selected, &nested_cwd)
        .unwrap_err()
        .to_string()
        .contains("is a symlink"));
}
