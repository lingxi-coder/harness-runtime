//! Captured runner API inputs and durable JSONL against the pinned 2.1.286
//! Lv/Fl/Ye lifecycle. The mock seam does not claim provider-wire preparation.
use super::*;
use lingxi_core::host::instructions::{
    InstructionContext, InstructionContextProvider, InstructionFile, InstructionFileType,
    InstructionReadContext, InstructionRendering, InstructionScope,
};
use std::path::Path;

struct InstructionFixture {
    full: Result<String, String>,
    root_user_context: std::collections::BTreeMap<String, String>,
    root_user_context_order: Vec<String>,
    managed: Result<String, String>,
    configured_managed_only: bool,
    loads: Mutex<Vec<InstructionScope>>,
    nested_scopes: Mutex<Vec<bool>>,
}

#[async_trait]
impl InstructionContextProvider for InstructionFixture {
    async fn load(&self, _: &Path, scope: InstructionScope) -> Result<InstructionContext, String> {
        self.loads.lock().unwrap().push(scope);
        let body = match scope {
            InstructionScope::Full if self.configured_managed_only => self.managed.clone()?,
            InstructionScope::Full => self.full.clone()?,
            InstructionScope::ManagedOnly => self.managed.clone()?,
        };
        let mut context = InstructionContext::default();
        // Both scopes project the same complete root Gv result; omit policy
        // does not rediscover fields from the child's inherited snapshot.
        context.user_context = self.root_user_context.clone();
        context.user_context_order = self.root_user_context_order.clone();
        context.user_context.remove("instructions");
        context.managed_instructions_only = self.configured_managed_only;
        let managed = scope == InstructionScope::ManagedOnly || self.configured_managed_only;
        let mut files = Vec::new();
        if let Ok(body) = &self.managed {
            if !body.is_empty() {
                files.push(InstructionFile {
                    path: "/managed/LINGXI.md".into(),
                    kind: InstructionFileType::Managed,
                    content: body.clone(),
                });
            }
        }
        if !managed {
            files.push(InstructionFile {
                path: "/project/LINGXI.md".into(),
                kind: InstructionFileType::Project,
                content: "FULL POLICY".into(),
            });
        }
        context.eager_instructions = Some(files);
        if !body.is_empty() {
            context.user_context.insert("instructions".into(), body);
        }
        Ok(context)
    }

    async fn after_read(
        &self,
        _: &Path,
        path: &Path,
        _: bool,
        context: &mut InstructionContext,
    ) -> InstructionReadContext {
        self.nested_scopes
            .lock()
            .unwrap()
            .push(context.managed_instructions_only);
        if context.managed_instructions_only
            || !context
                .sent_paths
                .insert(path.parent().unwrap().join("AGENTS.md"))
        {
            InstructionReadContext::default()
        } else {
            InstructionReadContext {
                agents_context: vec![
                    "Contents of /project/pkg/AGENTS.md:\n\nNESTED AGENTS POLICY".into(),
                ],
                ..Default::default()
            }
        }
    }
}

fn fixture(outcome: &str) -> Arc<InstructionFixture> {
    Arc::new(InstructionFixture {
        full: Ok("FULL POLICY".into()),
        root_user_context: serde_json::from_value(serde_json::json!({
            "instructions": "FULL POLICY",
            "userEmail": "user@example.test",
            "currentDate": "2026-09-30"
        }))
        .unwrap(),
        root_user_context_order: vec![
            "instructions".into(),
            "userEmail".into(),
            "currentDate".into(),
        ],
        managed: match outcome {
            "failure" => Err("unavailable".into()),
            "empty" => Ok(String::new()),
            _ => Ok("MANAGED POLICY".into()),
        },
        configured_managed_only: false,
        loads: Mutex::new(Vec::new()),
        nested_scopes: Mutex::new(Vec::new()),
    })
}

fn failed_full_fixture() -> Arc<InstructionFixture> {
    Arc::new(InstructionFixture {
        full: Err("initial Gv failed".into()),
        ..Arc::try_unwrap(fixture("managed")).ok().unwrap()
    })
}

async fn capture(
    mut context: SubagentContext,
    responses: Vec<Result<llm_runtime::HistoryResponse, llm_runtime::LlmError>>,
) -> Vec<ConversationMessage> {
    let api = MockSubagentApiClient::new(responses);
    context.api_client = Some(api.clone());
    context.agent_definition.max_turns = 3;
    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(64);
    Box::pin(run_subagent(context, event_rx, out_tx)).await;
    let _ = drain(out_rx).await;
    assert!(
        api.call_count() > 0,
        "must reach the model request boundary"
    );
    api.last_messages()
}

fn model_text(messages: &[ConversationMessage]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::User { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn omit_instruction_requests_match_native_oracle() {
    // Native fixtures keep their captured bytes; project the product instruction key here.
    let oracle: serde_json::Value = serde_json::from_str(
        &include_str!("../tests/fixtures/omit_claude_md_2_1_286.json")
            .replace("claudeMd", "instructions"),
    )
    .unwrap();
    for case in oracle["cases"].as_array().unwrap() {
        let provider = fixture(case["result"].as_str().unwrap());
        let mut ctx = fresh_subagent_ctx();
        ctx.agent_definition.omit_instructions = true;
        ctx.agent_definition.source = match case["source"].as_str().unwrap() {
            "built-in" => AgentSource::BuiltIn,
            "policySettings" => AgentSource::Settings(lingxi_core::types::SettingsScope::Managed),
            "plugin" => AgentSource::Plugin,
            _ => AgentSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        ctx.instruction_context.user_context =
            serde_json::from_value(case["original"].clone()).unwrap();
        ctx.instruction_provider = Some(provider);
        let selected = crate::instructions::resolve(&ctx).await.unwrap();
        let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
        let text = model_text(&messages);
        let expected: std::collections::BTreeMap<String, String> =
            serde_json::from_value(case["expected"]["userContext"].clone()).unwrap();
        for (key, value) in &expected {
            if key == "currentDate" {
                assert!(text.contains("Today's date is"), "{case}");
            } else if key == "instructions" {
                assert!(text.contains(value), "{case}");
            } else {
                assert!(text.contains(&format!("# {key}\n{value}")), "{case}");
            }
        }
        assert_eq!(
            text.contains("Contents of /project/LINGXI.md")
                || text.contains("Contents of /managed/LINGXI.md"),
            expected.contains_key("instructions"),
            "{case}"
        );
        assert_eq!(
            selected.managed_instructions_only,
            case["expected"]["managedInstructionsOnly"]
                .as_bool()
                .unwrap(),
            "{case}"
        );
    }
}

#[tokio::test]
async fn ordinary_child_receives_full_instruction_context_across_provider_profiles() {
    for profile in ["anthropic", "openai", "gemini"] {
        let mut ctx = fresh_subagent_ctx();
        ctx.model_profile = Some(profile.into());
        ctx.instruction_provider = Some(fixture("managed"));
        let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
        assert!(model_text(&messages).contains("Contents of /project/LINGXI.md (project instructions, checked into the codebase):\n\nFULL POLICY"));
    }
}

#[tokio::test]
async fn current_root_full_context_replaces_stale_parent_fields_and_order() {
    for cold_resume in [false, true] {
        for omit in [false, true] {
            for root_order in [
                vec![
                    "rootField".into(),
                    "currentDate".into(),
                    "instructions".into(),
                ],
                Vec::new(),
            ] {
                let provider = Arc::new(InstructionFixture {
                    root_user_context: serde_json::from_value(serde_json::json!({
                        "instructions": "FULL POLICY",
                        "rootField": "CURRENT ROOT VALUE",
                        "currentDate": "2026-10-01"
                    }))
                    .unwrap(),
                    root_user_context_order: root_order.clone(),
                    ..Arc::try_unwrap(fixture("managed")).ok().unwrap()
                });
                let mut ctx = fresh_subagent_ctx();
                ctx.agent_definition.source = AgentSource::Plugin;
                ctx.agent_definition.omit_instructions = omit;
                ctx.instruction_context.user_context = serde_json::from_value(serde_json::json!({
                    "instructions": "STALE INHERITED POLICY",
                    "userEmail": "logged-out@example.test",
                    "attachedProject": "DETACHED PARENT PROJECT",
                    "customParentField": "STALE PARENT VALUE",
                    "currentDate": "2026-09-30"
                }))
                .unwrap();
                ctx.instruction_context.user_context_order = vec![
                    "userEmail".into(),
                    "attachedProject".into(),
                    "customParentField".into(),
                    "currentDate".into(),
                    "instructions".into(),
                ];
                ctx.instruction_provider = Some(provider);
                if cold_resume {
                    ctx.resumed_history = Some(vec![ConversationMessage::user(
                        MessageId::new(),
                        "persisted original task".into(),
                    )]);
                    ctx.instruction_context_is_override = true;
                }

                let selected = crate::instructions::resolve(&ctx).await.unwrap();
                assert_eq!(selected.user_context_order, root_order);
                assert_eq!(selected.user_context["currentDate"], "2026-10-01");
                assert_eq!(selected.user_context["rootField"], "CURRENT ROOT VALUE");
                assert_eq!(
                    selected.user_context["instructions"],
                    if omit {
                        "MANAGED POLICY"
                    } else {
                        "FULL POLICY"
                    }
                );
                for stale_key in ["userEmail", "attachedProject", "customParentField"] {
                    assert!(!selected.user_context.contains_key(stale_key));
                }
                let messages =
                    capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
                let text = model_text(&messages);
                assert!(text.contains("CURRENT ROOT VALUE"));
                for stale in [
                    "logged-out@example.test",
                    "DETACHED PARENT PROJECT",
                    "STALE PARENT VALUE",
                    "STALE INHERITED POLICY",
                    "# userEmail",
                    "# attachedProject",
                    "# customParentField",
                ] {
                    assert!(!text.contains(stale), "stale root field leaked: {stale}");
                }
            }
        }
    }
}

struct ChangedAccountProjection {
    full: Arc<InstructionFixture>,
    managed: Arc<InstructionFixture>,
}

#[async_trait]
impl InstructionContextProvider for ChangedAccountProjection {
    async fn load(
        &self,
        cwd: &Path,
        scope: InstructionScope,
    ) -> Result<InstructionContext, String> {
        let provider = match scope {
            InstructionScope::Full => &self.full,
            InstructionScope::ManagedOnly => &self.managed,
        };
        let mut context = provider.load(cwd, scope).await?;
        context.rendering = InstructionRendering::Inline;
        let path = match scope {
            InstructionScope::Full => "/project/selected/LINGXI.md",
            InstructionScope::ManagedOnly => "/managed/current/LINGXI.md",
        };
        context.sent_paths.insert(path.into());
        context.instructions_root = Path::new(path).parent().map(Path::to_path_buf);
        Ok(context)
    }
}

#[tokio::test]
async fn managed_projection_changes_only_instruction_body_after_account_change() {
    for outcome in ["managed", "empty"] {
        let full = Arc::new(InstructionFixture {
            root_user_context: serde_json::from_value(serde_json::json!({
                "instructions": "FULL POLICY",
                "userEmail": "selected-owner@example.test",
                "currentDate": "2026-10-01",
                "attachedProject": "SELECTED ROOT PROJECT",
                "fullOnlyField": "SELECTED ROOT VALUE"
            }))
            .unwrap(),
            root_user_context_order: vec![
                "attachedProject".into(),
                "userEmail".into(),
                "currentDate".into(),
                "fullOnlyField".into(),
                "instructions".into(),
            ],
            ..Arc::try_unwrap(fixture("managed")).ok().unwrap()
        });
        let managed = Arc::new(InstructionFixture {
            root_user_context: serde_json::from_value(serde_json::json!({
                "instructions": "FULL POLICY",
                "userEmail": "next-account@example.test",
                "currentDate": "2027-02-03",
                "attachedProject": "NEXT ROOT PROJECT",
                "managedOnlyField": "NEXT ROOT VALUE"
            }))
            .unwrap(),
            root_user_context_order: vec!["currentDate".into(), "userEmail".into()],
            ..Arc::try_unwrap(fixture(outcome)).ok().unwrap()
        });
        let provider = Arc::new(ChangedAccountProjection {
            full: full.clone(),
            managed: managed.clone(),
        });
        let mut ctx = fresh_subagent_ctx();
        ctx.agent_definition.source = AgentSource::Plugin;
        ctx.agent_definition.omit_instructions = true;
        ctx.instruction_provider = Some(provider);

        let selected = crate::instructions::resolve(&ctx).await.unwrap();
        let mut expected = full.root_user_context.clone();
        if outcome == "managed" {
            expected.insert("instructions".into(), "MANAGED POLICY".into());
        } else {
            expected.remove("instructions");
        }
        assert_eq!(selected.user_context, expected);
        assert_eq!(selected.user_context_order, full.root_user_context_order);
        assert_eq!(selected.managed_instructions_only, outcome == "managed");
        assert_eq!(
            selected.sent_paths,
            std::collections::HashSet::from(
                [Path::new("/managed/current/LINGXI.md").to_path_buf()]
            )
        );
        assert_eq!(
            selected.instructions_root.as_deref(),
            Some(Path::new("/managed/current"))
        );
        let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
        let text = model_text(&messages);
        for selected_field in [
            "# userEmail\nselected-owner@example.test",
            "# currentDate\n2026-10-01",
            "# attachedProject\nSELECTED ROOT PROJECT",
            "# fullOnlyField\nSELECTED ROOT VALUE",
        ] {
            assert!(
                text.contains(selected_field),
                "selected Full field changed: {selected_field}"
            );
        }
        for next_field in [
            "next-account@example.test",
            "2027-02-03",
            "NEXT ROOT PROJECT",
            "NEXT ROOT VALUE",
            "# managedOnlyField",
            "FULL POLICY",
        ] {
            assert!(
                !text.contains(next_field),
                "managed projection replaced Full: {next_field}"
            );
        }
        assert_eq!(text.contains("MANAGED POLICY"), outcome == "managed");
        assert_eq!(*full.loads.lock().unwrap(), vec![InstructionScope::Full; 2]);
        assert_eq!(
            *managed.loads.lock().unwrap(),
            vec![InstructionScope::ManagedOnly; 2]
        );
    }
}

#[tokio::test]
async fn failed_full_load_aborts_before_startup_and_releases_restore_gate() {
    use lingxi_core::host::handback::HandbackRestoreBatch;

    for cold_resume in [false, true] {
        for omit in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let provider = failed_full_fixture();
            let api = MockSubagentApiClient::new(vec![Ok(text_response(
                "must not be requested",
                Some("end_turn"),
            ))]);
            let mut ctx = fresh_subagent_ctx();
            let agent_id = ctx.agent_id;
            let transcript_path = directory.path().join(format!("agent-{agent_id}.jsonl"));
            let prior_transcript = b"retained transcript bytes\n";
            ctx.agent_definition.source = AgentSource::Plugin;
            ctx.agent_definition.omit_instructions = omit;
            ctx.instruction_context
                .user_context
                .insert("instructions".into(), "STALE INHERITED POLICY".into());
            ctx.instruction_provider = Some(provider.clone());
            ctx.api_client = Some(api.clone());
            ctx.transcript_subdir = directory.path().to_path_buf();
            ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
                directory.path().to_path_buf(),
            )));
            if cold_resume {
                std::fs::write(&transcript_path, prior_transcript).unwrap();
                ctx.resumed_history = Some(vec![ConversationMessage::user(
                    MessageId::new(),
                    "persisted original task".into(),
                )]);
                // Transcript restoration cannot turn a stale override into
                // the fresh explicit-fork bypass of the initial Gv Promise.
                ctx.instruction_context_is_override = true;
            }
            let sibling_id = lingxi_core::types::AgentId::new();
            let batch = HandbackRestoreBatch::new([agent_id, sibling_id]);
            ctx.handback_restore_start = batch.participant(agent_id);
            let (event_tx, event_rx) = mpsc::channel(1);
            drop(event_tx);
            let (out_tx, out_rx) = mpsc::channel(64);
            Box::pin(run_subagent(ctx, event_rx, out_tx)).await;
            let events = drain(out_rx).await;

            assert_eq!(events.len(), 1, "startup publishes only its failure");
            let SubagentEvent::Failed {
                agent_id: failed_id,
                error,
                cumulative_usage,
            } = &events[0]
            else {
                panic!("initial Gv rejection must fail the child: {events:?}");
            };
            assert_eq!(*failed_id, agent_id);
            assert_eq!(error, "initial Gv failed");
            assert_eq!(cumulative_usage.counts().input_tokens, 0);
            assert_eq!(cumulative_usage.counts().output_tokens, 0);
            assert_eq!(api.call_count(), 0);
            assert_eq!(
                *provider.loads.lock().unwrap(),
                vec![InstructionScope::Full]
            );
            if cold_resume {
                assert_eq!(std::fs::read(&transcript_path).unwrap(), prior_transcript);
            } else {
                assert!(!transcript_path.exists());
            }
            let sibling = batch.participant(sibling_id).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), sibling.arrive_and_wait())
                .await
                .expect("a failed initial load releases its cold-restore startup slot");
        }
    }
}

#[tokio::test]
async fn unconfigured_api_fails_before_discovery_or_startup_persistence() {
    use lingxi_core::host::handback::HandbackRestoreBatch;

    for cold_resume in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let provider = fixture("managed");
        let mut ctx = fresh_subagent_ctx();
        let agent_id = ctx.agent_id;
        let transcript_path = directory.path().join(format!("agent-{agent_id}.jsonl"));
        let previous = b"durable previous transcript\n";
        ctx.instruction_provider = Some(provider.clone());
        ctx.transcript_subdir = directory.path().to_path_buf();
        ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
            directory.path().to_path_buf(),
        )));
        if cold_resume {
            std::fs::write(&transcript_path, previous).unwrap();
            ctx.resumed_history = Some(vec![ConversationMessage::user(
                MessageId::new(),
                "previous task".into(),
            )]);
        }
        let sibling_id = lingxi_core::types::AgentId::new();
        let batch = HandbackRestoreBatch::new([agent_id, sibling_id]);
        ctx.handback_restore_start = batch.participant(agent_id);
        let (event_tx, event_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(64);
        run_subagent(ctx, event_rx, out_tx).await;
        drop(event_tx);
        let events = drain(out_rx).await;
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], SubagentEvent::Failed { agent_id: failed_id, error, .. }
            if *failed_id == agent_id && error == "Subagent model API is not configured")
        );
        assert!(provider.loads.lock().unwrap().is_empty());
        if cold_resume {
            assert_eq!(std::fs::read(&transcript_path).unwrap(), previous);
        } else {
            assert!(!transcript_path.exists());
        }
        let sibling = batch.participant(sibling_id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), sibling.arrive_and_wait())
            .await
            .expect("missing API configuration releases its restore slot");
    }
}

#[tokio::test]
async fn unconfigured_api_failure_releases_the_one_shot_pool_slot() {
    use lingxi_core::host::subagent_spawn::{
        SubagentInheritance, SubagentResult, SubagentSpawnRequest, SubagentSpawner,
    };

    let pool = Arc::new(crate::StateMachinePool::new(
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        1,
    ));
    let spawner = crate::PoolSubagentSpawner::new(pool.clone());
    for _ in 0..2 {
        let request: SubagentSpawnRequest = serde_json::from_value(serde_json::json!({
            "subagent_type": "general-purpose",
            "prompt": "must not fabricate a completion"
        }))
        .unwrap();
        let result = spawner
            .spawn(
                request,
                SubagentInheritance {
                    tool_invoker: CountingInvoker::new(),
                    budget: Arc::new(MockBudget { exceeded: false }),
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(result, SubagentResult::Failed { reason, usage, .. }
            if reason == "Subagent model API is not configured" && usage.total_tokens == 0)
        );
        assert_eq!(pool.slot_count().await, 0);
    }
}

#[tokio::test]
async fn failed_full_load_uses_the_one_shot_terminal_cleanup_path() {
    use lingxi_core::host::subagent_spawn::{
        SubagentInheritance, SubagentResult, SubagentSpawnRequest, SubagentSpawner,
    };

    let provider = failed_full_fixture();
    let api = MockSubagentApiClient::new(Vec::new());
    let pool = Arc::new(crate::StateMachinePool::new(
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        1,
    ));
    let spawner = crate::PoolSubagentSpawner::new(pool.clone())
        .with_api_client(api.clone())
        .with_instruction_provider(provider.clone());
    for _ in 0..2 {
        let request: SubagentSpawnRequest = serde_json::from_value(serde_json::json!({
            "subagent_type": "general-purpose",
            "prompt": "initial policy is required"
        }))
        .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            spawner.spawn(
                request,
                SubagentInheritance {
                    tool_invoker: CountingInvoker::new(),
                    budget: Arc::new(MockBudget { exceeded: false }),
                },
            ),
        )
        .await
        .expect("initial-load failure reaches the normal spawn terminal path")
        .unwrap();
        assert!(
            matches!(result, SubagentResult::Failed { reason, usage, .. }
            if reason == "initial Gv failed" && usage.total_tokens == 0)
        );
        assert_eq!(pool.slot_count().await, 0);
    }
    assert_eq!(api.call_count(), 0);
    assert_eq!(
        *provider.loads.lock().unwrap(),
        vec![InstructionScope::Full, InstructionScope::Full]
    );
}

#[tokio::test]
async fn failed_managed_projection_retains_the_successful_full_load() {
    let provider = fixture("failure");
    let mut ctx = fresh_subagent_ctx();
    ctx.agent_definition.source = AgentSource::Plugin;
    ctx.agent_definition.omit_instructions = true;
    ctx.instruction_context
        .user_context
        .insert("instructions".into(), "STALE INHERITED POLICY".into());
    ctx.instruction_provider = Some(provider.clone());
    let selected = crate::instructions::resolve(&ctx).await.unwrap();
    assert_eq!(selected.user_context["instructions"], "FULL POLICY");
    assert!(!selected.managed_instructions_only);
    assert_eq!(
        *provider.loads.lock().unwrap(),
        vec![InstructionScope::Full, InstructionScope::ManagedOnly]
    );
    let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
    let text = model_text(&messages);
    assert!(text.contains("FULL POLICY"));
    assert!(!text.contains("STALE INHERITED POLICY"));
}

#[tokio::test]
async fn absent_provider_preserves_explicit_context_without_discovery() {
    let mut ctx = fresh_subagent_ctx();
    ctx.instruction_context.rendering = InstructionRendering::Inline;
    ctx.instruction_context
        .user_context
        .insert("instructions".into(), "CALLER SUPPLIED POLICY".into());
    let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
    assert!(model_text(&messages).contains("CALLER SUPPLIED POLICY"));
}

#[tokio::test]
async fn explicit_fork_context_bypasses_omit_and_preserves_sent_cursor() {
    let provider = failed_full_fixture();
    let mut ctx = fresh_subagent_ctx();
    ctx.agent_definition.omit_instructions = true;
    ctx.agent_definition.source = AgentSource::Plugin;
    ctx.instruction_provider = Some(provider.clone());
    ctx.instruction_context_is_override = true;
    ctx.instruction_context.rendering = InstructionRendering::Inline;
    ctx.instruction_context.managed_instructions_only = true;
    ctx.instruction_context
        .user_context
        .insert("instructions".into(), "EXPLICIT FORK POLICY".into());
    ctx.instruction_context
        .sent_paths
        .insert("/project/nested/AGENTS.md".into());
    ctx.fork_context_messages = Some(vec![ConversationMessage::user(
        MessageId::new(),
        "fork directive".into(),
    )]);
    let selected = crate::instructions::resolve(&ctx).await.unwrap();
    let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
    assert_eq!(
        model_text(&messages)
            .matches("EXPLICIT FORK POLICY")
            .count(),
        1
    );
    assert!(provider.loads.lock().unwrap().is_empty());
    assert!(!selected.managed_instructions_only);
    assert!(
        selected
            .sent_paths
            .contains(Path::new("/project/nested/AGENTS.md"))
    );
}

#[tokio::test]
async fn cold_resume_reloads_current_managed_policy_without_persisted_selection() {
    let provider = fixture("managed");
    let provider = Arc::new(InstructionFixture {
        managed: Ok("CURRENT MANAGED POLICY".into()),
        ..Arc::try_unwrap(provider).ok().unwrap()
    });
    let mut resumed = fresh_subagent_ctx();
    resumed.agent_definition.source = AgentSource::Plugin;
    resumed.agent_definition.omit_instructions = true;
    resumed.resumed_history = Some(vec![ConversationMessage::user(
        MessageId::new(),
        "original task".into(),
    )]);
    // Even a stale private fork carrier cannot turn transcript restore into
    // an explicit userContext override (jt passes no such override to Lv).
    resumed.instruction_context_is_override = true;
    resumed.instruction_context.managed_instructions_only = true;
    resumed
        .instruction_context
        .user_context
        .insert("instructions".into(), "OLD MANAGED POLICY".into());
    resumed
        .instruction_context
        .sent_paths
        .insert("/project/src/AGENTS.md".into());
    resumed.instruction_provider = Some(provider.clone());
    resumed.tool_invoker = Some(CountingInvoker::new());
    let mut response = tool_use_response("Read", Some("tool_use"));
    if let llm_runtime::ContentBlock::ToolCall { input, .. } = &mut response.content[0] {
        *input = serde_json::json!({"file_path":"/project/src/file.rs"});
    }
    let messages = capture(
        resumed,
        vec![Ok(response), Ok(text_response("done", Some("end_turn")))],
    )
    .await;
    assert!(
        *provider.loads.lock().unwrap()
            == vec![InstructionScope::Full, InstructionScope::ManagedOnly],
        "cold resume resolves current Gv/Fl selection"
    );
    assert_eq!(*provider.nested_scopes.lock().unwrap(), vec![true]);
    let text = model_text(&messages);
    assert_eq!(text.matches("CURRENT MANAGED POLICY").count(), 1);
    assert!(!text.contains("OLD MANAGED POLICY"));
    assert!(!text.contains("FULL POLICY"));
    assert!(!text.contains("NESTED AGENTS POLICY"));
}

#[tokio::test]
async fn fork_of_main_session_injects_its_outgoing_only_instruction_prefix() {
    let provider = fixture("failure");
    let mut ctx = fresh_subagent_ctx();
    ctx.agent_definition.omit_instructions = true;
    ctx.agent_definition.source = AgentSource::Plugin;
    ctx.instruction_provider = Some(provider.clone());
    ctx.instruction_context_is_override = true;
    ctx.instruction_context.rendering = InstructionRendering::Inline;
    ctx.instruction_context
        .user_context
        .insert("instructions".into(), "MAIN SNAPSHOT POLICY".into());
    ctx.fork_context_messages = Some(vec![ConversationMessage::user(
        MessageId::new(),
        "fork directive".into(),
    )]);
    let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
    assert_eq!(
        model_text(&messages)
            .matches("MAIN SNAPSHOT POLICY")
            .count(),
        1
    );
    assert!(provider.loads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fresh_child_scope_selection_matches_native_lv_oracle() {
    let oracle: serde_json::Value = serde_json::from_str(
        &include_str!("../tests/fixtures/omit_claude_md_2_1_286.json")
            .replace("claudeMd", "instructions"),
    )
    .unwrap();
    for case in oracle["selectionCases"].as_array().unwrap() {
        let provider = fixture(case["result"].as_str().unwrap());
        let mut ctx = fresh_subagent_ctx();
        ctx.agent_definition.source = AgentSource::Plugin;
        ctx.agent_definition.omit_instructions = case["omit"].as_bool().unwrap();
        ctx.instruction_context_is_override = case["explicit"].as_bool().unwrap();
        ctx.instruction_context.managed_instructions_only =
            case["inheritedManaged"].as_bool().unwrap();
        ctx.instruction_context.user_context =
            serde_json::from_value(case["original"].clone()).unwrap();
        ctx.instruction_provider = Some(provider.clone());
        let selected = crate::instructions::resolve(&ctx).await.unwrap();
        provider.loads.lock().unwrap().clear();
        let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
        assert_eq!(
            serde_json::to_value(&selected.user_context).unwrap(),
            case["expected"]["userContext"],
            "{case}"
        );
        assert_eq!(
            selected.managed_instructions_only,
            case["expected"]["managedInstructionsOnly"]
                .as_bool()
                .unwrap(),
            "{case}"
        );
        let text = model_text(&messages);
        for (key, value) in &selected.user_context {
            if key == "currentDate" {
                assert!(text.contains("Today's date is"), "{case}");
            } else if key == "instructions" {
                assert!(text.contains(value), "{case}");
            } else {
                assert!(text.contains(&format!("# {key}\n{value}")), "{case}");
            }
        }
        let expected_loads: Vec<_> = case["loads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|scope| match scope.as_str().unwrap() {
                "Full" => InstructionScope::Full,
                "ManagedOnly" => InstructionScope::ManagedOnly,
                other => panic!("unexpected native scope {other}"),
            })
            .collect();
        assert_eq!(*provider.loads.lock().unwrap(), expected_loads, "{case}");
    }
}

#[tokio::test]
async fn ordinary_child_respects_trusted_provider_managed_only_mode() {
    let provider = fixture("managed");
    let provider = Arc::new(InstructionFixture {
        configured_managed_only: true,
        ..Arc::try_unwrap(provider).ok().unwrap()
    });
    let mut ctx = fresh_subagent_ctx();
    ctx.instruction_provider = Some(provider.clone());
    let selected = crate::instructions::resolve(&ctx).await.unwrap();
    provider.loads.lock().unwrap().clear();
    let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
    assert_eq!(
        *provider.loads.lock().unwrap(),
        vec![InstructionScope::Full]
    );
    assert!(model_text(&messages).contains("MANAGED POLICY"));
    assert!(!model_text(&messages).contains("FULL POLICY"));
    assert!(selected.managed_instructions_only);
}

struct InstructionForkInvoker {
    agent_call: Mutex<Option<lingxi_core::host::tool_invoker::SubagentInvocationContext>>,
}

#[async_trait]
impl lingxi_core::host::tool_invoker::ToolInvoker for InstructionForkInvoker {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn invoke(
        &self,
        name: &str,
        _: serde_json::Value,
        context: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        if name == "Agent" {
            *self.agent_call.lock().unwrap() = Some(context);
        }
        Ok(serde_json::json!({}))
    }
}

#[tokio::test]
async fn nested_read_then_fork_retains_policy_body_and_cursor_without_reemitting() {
    let provider = fixture("managed");
    let invoker = Arc::new(InstructionForkInvoker {
        agent_call: Mutex::new(None),
    });
    let read_response = || {
        let mut response = tool_use_response("Read", Some("tool_use"));
        if let llm_runtime::ContentBlock::ToolCall { input, .. } = &mut response.content[0] {
            *input = serde_json::json!({"file_path":"/project/pkg/file.rs"});
        }
        response
    };
    let mut parent = fresh_subagent_ctx();
    let transcripts = tempfile::tempdir().unwrap();
    let parent_id = parent.agent_id;
    parent.transcript_subdir = transcripts.path().to_path_buf();
    parent.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        transcripts.path().to_path_buf(),
    )));
    parent.instruction_provider = Some(provider.clone());
    parent.tool_invoker = Some(invoker.clone());
    parent.rendered_system_prompt = Some(Arc::from("EXACT PARENT SYSTEM"));
    let first_read = read_response();
    let read_id = match &first_read.content[0] {
        llm_runtime::ContentBlock::ToolCall { id, .. } => id.clone(),
        other => panic!("expected Read tool call, got {other:?}"),
    };
    let parent_messages = capture(
        parent,
        vec![
            Ok(first_read),
            Ok(tool_use_response("Agent", Some("tool_use"))),
            Ok(text_response("done", Some("end_turn"))),
        ],
    )
    .await;
    let rendered = "<system-reminder>\ntool.call hook additional context: Contents of /project/pkg/AGENTS.md:\n\nNESTED AGENTS POLICY\n</system-reminder>";
    assert!(parent_messages.iter().any(|message| matches!(message,
        ConversationMessage::User { is_meta: true, content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == rendered)))));
    let rows: Vec<serde_json::Value> =
        std::fs::read_to_string(transcripts.path().join(format!("agent-{parent_id}.jsonl")))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let attachment_rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            row["type"] == "attachment" && row["attachment"]["type"] == "hook_additional_context"
        })
        .collect();
    assert_eq!(attachment_rows.len(), 1);
    assert_eq!(
        attachment_rows[0]["attachment"],
        serde_json::json!({
            "type": "hook_additional_context",
            "content": ["Contents of /project/pkg/AGENTS.md:\n\nNESTED AGENTS POLICY"],
            "hookName": "tool.call",
            "toolUseID": format!("{read_id}-context"),
            "hookEvent": "PostToolUse"
        })
    );
    let restored_history: Vec<ConversationMessage> = rows
        .iter()
        .filter_map(|row| {
            row.get("message")
                .filter(|message| !message.is_null())
                .cloned()
        })
        .map(|message| serde_json::from_value(message).unwrap())
        .collect();
    assert_eq!(
        model_text(&restored_history)
            .matches("NESTED AGENTS POLICY")
            .count(),
        1
    );
    assert!(
        !rows
            .iter()
            .any(|row| row["message"]["subtype"] == "instruction_context")
    );
    assert_eq!(
        model_text(&restored_history).matches("FULL POLICY").count(),
        1
    );
    assert!(!model_text(&restored_history).contains("# instructions"));
    let invocation = invoker.agent_call.lock().unwrap().take().unwrap();
    let fork = invocation.fork_context.unwrap();
    assert_eq!(fork.system_prompt.as_deref(), Some("EXACT PARENT SYSTEM"));
    assert_eq!(model_text(&fork.messages).matches("FULL POLICY").count(), 1);
    let mut child = fresh_subagent_ctx();
    child.instruction_context = invocation.instruction_context.unwrap();
    assert!(
        child
            .instruction_context
            .sent_paths
            .contains(Path::new("/project/pkg/AGENTS.md"))
    );
    child.instruction_context_is_override = true;
    child.fork_context_messages = Some(lingxi_core::host::fork_subagent::build_forked_context(
        "fork directive",
        &fork.messages,
    ));
    child.instruction_provider = Some(provider.clone());
    child.tool_invoker = Some(invoker);
    let messages = capture(
        child,
        vec![
            Ok(read_response()),
            Ok(text_response("done", Some("end_turn"))),
        ],
    )
    .await;
    assert_eq!(
        model_text(&messages)
            .matches("NESTED AGENTS POLICY")
            .count(),
        1
    );
    assert_eq!(
        *provider.loads.lock().unwrap(),
        vec![InstructionScope::Full]
    );

    // Ye's sent-path Map is private process state. Replaying its historical
    // context attachment after a cold restore does not restore that Map.
    let mut resumed = fresh_subagent_ctx();
    resumed.resumed_history = Some(restored_history);
    resumed.instruction_context.announcement_history = rows
        .iter()
        .filter_map(|row| row.get("attachment").cloned())
        .filter(|attachment| {
            matches!(
                attachment["type"].as_str(),
                Some("instructions" | "session_context" | "context_sections" | "date")
            )
        })
        .collect();
    resumed.instruction_provider = Some(provider.clone());
    resumed.tool_invoker = Some(CountingInvoker::new());
    let messages = capture(
        resumed,
        vec![
            Ok(read_response()),
            Ok(text_response("done", Some("end_turn"))),
        ],
    )
    .await;
    assert_eq!(
        model_text(&messages)
            .matches("NESTED AGENTS POLICY")
            .count(),
        2,
        "cold resume re-announces the previously read nested instructions"
    );
    assert_eq!(model_text(&messages).matches("FULL POLICY").count(), 1);
}

#[test]
fn instruction_fork_carrier_is_private_to_the_live_process() {
    use lingxi_core::host::subagent_spawn::SubagentSpawnRequest;
    let mut context = InstructionContext::default();
    context.rendering = InstructionRendering::Inline;
    context
        .user_context
        .insert("instructions".into(), "FROZEN LIVE POLICY".into());
    context.sent_paths.insert("/project/pkg/AGENTS.md".into());
    let request = SubagentSpawnRequest {
        instruction_context: Some(context.clone()),
        ..Default::default()
    };
    assert_eq!(request.clone().instruction_context, Some(context));
    let persisted = serde_json::to_value(request).unwrap();
    assert!(persisted.get("instruction_context").is_none());
    let restored: SubagentSpawnRequest = serde_json::from_value(persisted).unwrap();
    assert!(restored.instruction_context.is_none());
}

#[tokio::test]
async fn cold_child_recovers_native_inline_hint_without_persisting_its_prefix() {
    let mut ctx = fresh_subagent_ctx();
    let directory = tempfile::tempdir().unwrap();
    let id = ctx.agent_id;
    ctx.transcript_subdir = directory.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        directory.path().to_path_buf(),
    )));
    ctx.resumed_history = Some(vec![ConversationMessage::user(
        MessageId::new(),
        "original task".into(),
    )]);
    ctx.instruction_context.announcement_history.push(serde_json::json!({
        "type":"prompt_snapshot","systemPrompt":["captured system"],"tools":[],"contextRendering":"inline"
    }));
    ctx.instruction_context
        .user_context
        .insert("instructions".into(), "STALE PRIVATE POLICY".into());
    ctx.instruction_provider = Some(fixture("managed"));
    let messages = capture(ctx, vec![Ok(text_response("done", Some("end_turn")))]).await;
    let text = model_text(&messages);
    assert!(text.contains("# instructions\nFULL POLICY"));
    assert!(!text.contains("STALE PRIVATE POLICY"));
    let saved =
        std::fs::read_to_string(directory.path().join(format!("agent-{id}.jsonl"))).unwrap();
    assert!(!saved.contains("FULL POLICY"));
    assert!(!saved.contains("instruction_context"));
    assert!(
        !saved
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .any(|row| row["attachment"]["type"] == "instructions")
    );
}

#[test]
fn cold_child_snapshot_routing_matches_native_schema_and_independent_gate() {
    let fixture: serde_json::Value = serde_json::from_str(
        &include_str!("../../core/tests/fixtures/instruction_announcements_2_1_286.json")
            .replace("claudeMd", "instructions"),
    )
    .unwrap();
    let cases = fixture["snapshotCases"].as_array().unwrap();
    assert_eq!(cases.len(), 26);
    for case in cases {
        let attachments: Vec<_> = case["prior"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row.get("attachment").cloned())
            .collect();
        let enabled = case["expected"]["metadataEnabled"].as_bool().unwrap();
        let expected = if case["expected"]["hint"] == "inline" {
            InstructionRendering::Inline
        } else {
            InstructionRendering::Announced
        };
        assert_eq!(
            crate::instructions::cold_rendering(&attachments, enabled),
            expected,
            "{}",
            case["name"]
        );
    }
}

#[tokio::test]
async fn parked_live_child_retains_instruction_selection_and_sent_cursor() {
    let provider = fixture("managed");
    let read = || {
        let mut response = tool_use_response("Read", Some("tool_use"));
        if let llm_runtime::ContentBlock::ToolCall { input, .. } = &mut response.content[0] {
            *input = serde_json::json!({"file_path":"/project/pkg/file.rs"});
        }
        Ok(response)
    };
    let api = MockSubagentApiClient::new(vec![
        read(),
        Ok(text_response("first", Some("end_turn"))),
        read(),
        Ok(text_response("second", Some("end_turn"))),
    ]);
    let mut ctx = fresh_subagent_ctx();
    ctx.persistent = true;
    ctx.agent_definition.max_turns = 2;
    ctx.api_client = Some(api.clone());
    ctx.instruction_provider = Some(provider.clone());
    ctx.tool_invoker = Some(CountingInvoker::new());
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(64);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        for turn in 0..2 {
            loop {
                match out_rx.recv().await.expect("live runner remains parked") {
                    SubagentEvent::Completed { .. } => break,
                    SubagentEvent::Failed { error, .. } => panic!("live child failed: {error}"),
                    _ => {}
                }
            }
            if turn == 0 {
                event_tx
                    .send(lingxi_core::Event::UserMessage {
                        message_id: MessageId::new(),
                        request_id: RequestId::new(),
                        content: "read the same file again".into(),
                    })
                    .await
                    .unwrap();
            }
        }
    })
    .await
    .expect("both live turn sets park");
    drop(event_tx);
    runner.await.unwrap();
    assert_eq!(api.call_count(), 4);
    assert_eq!(
        *provider.loads.lock().unwrap(),
        vec![InstructionScope::Full]
    );
    assert_eq!(*provider.nested_scopes.lock().unwrap(), vec![false, false]);
    let text = model_text(&api.last_messages());
    assert_eq!(text.matches("FULL POLICY").count(), 1);
    assert_eq!(text.matches("NESTED AGENTS POLICY").count(), 1);
}

#[test]
fn outgoing_instruction_prefix_preserves_the_mandatory_task_under_input_cap() {
    let mut context = InstructionContext::default();
    context.rendering = InstructionRendering::Inline;
    context
        .user_context
        .insert("instructions".into(), "POLICY".into());
    let task = ConversationMessage::user(MessageId::new(), "TASK".into());
    let newest = ConversationMessage::user(MessageId::new(), "LATEST".into());
    let history = vec![
        task.clone(),
        ConversationMessage::user(MessageId::new(), "optional".repeat(100)),
        newest.clone(),
    ];
    let required =
        instruction_request_messages(&[task.clone(), newest.clone()], &context, None).unwrap();
    let cap = serde_json::to_vec(&required).unwrap().len() as u64;
    let messages = instruction_request_messages(&history, &context, Some(cap)).unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1], task);
    assert_eq!(messages[2], newest);
    assert!(serde_json::to_vec(&messages).unwrap().len() as u64 <= cap);
}
