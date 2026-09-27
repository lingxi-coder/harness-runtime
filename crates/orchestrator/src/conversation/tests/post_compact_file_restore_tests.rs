use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use tool_api::read_file_state::{set, ReadFileEntry};
use tool_api::registry::ToolRegistry;
use tracing::field::Field;
use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::Registry;

fn stale_entry(content: &str) -> ReadFileEntry {
    ReadFileEntry {
        content: content.to_string(),
        mtime_ms: 1,
        offset: None,
        limit: None,
        from_read: true,
        seeded_from_context: false,
        is_partial_view: false,
    }
}

/// Serialize + clean the process-global invoked-skill registry so these
/// tests (which now exercise the skill arm too) don't see rows registered by
/// a parallel test. Resets on acquire AND on drop (under the lock) so no row
/// leaks past the test. Hold the returned guard for the whole test body.
struct RegistryGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
impl Drop for RegistryGuard {
    fn drop(&mut self) {
        compaction::invoked_skills::reset_for_test();
    }
}
fn registry_guard() -> RegistryGuard {
    let g = compaction::invoked_skills::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    compaction::invoked_skills::reset_for_test();
    RegistryGuard(g)
}

async fn orch_with_bus(
    cwd: std::path::PathBuf,
    map: tool_api::read_file_state::ReadFileStateMap,
    sink: Arc<telemetry::InMemorySink>,
) -> ConversationOrchestrator {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    bus.attach_sink(sink).await;
    let config = OrchestratorConfig {
        plans_directory: Some("plans".to_string()),
        ..OrchestratorConfig::default()
    };
    ConversationOrchestrator::new(
        config,
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        cwd,
    )
    .with_analytics_bus(bus)
    .with_read_state_map(map)
}

fn restore_names(events: &[telemetry::RecordedEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.name.starts_with("tengu_post_compact_file_restore"))
        .map(|e| e.name.clone())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedTelemetryEvent {
    event: String,
    fields: BTreeMap<String, String>,
}

#[derive(Default, Clone)]
struct TelemetryEventCapture {
    events: Arc<StdMutex<Vec<CapturedTelemetryEvent>>>,
}

impl<S: Subscriber> Layer<S> for TelemetryEventCapture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        struct V {
            event: Option<String>,
            fields: BTreeMap<String, String>,
        }

        impl tracing::field::Visit for V {
            fn record_bool(&mut self, field: &Field, value: bool) {
                if field.name() != "event" {
                    self.fields
                        .insert(field.name().to_string(), value.to_string());
                }
            }

            fn record_i64(&mut self, field: &Field, value: i64) {
                if field.name() != "event" {
                    self.fields
                        .insert(field.name().to_string(), value.to_string());
                }
            }

            fn record_u64(&mut self, field: &Field, value: u64) {
                if field.name() != "event" {
                    self.fields
                        .insert(field.name().to_string(), value.to_string());
                }
            }

            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "event" {
                    self.event = Some(value.to_string());
                } else {
                    self.fields
                        .insert(field.name().to_string(), value.to_string());
                }
            }

            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}").trim_matches('"').to_string();
                if field.name() == "event" {
                    self.event = Some(rendered);
                } else {
                    self.fields.insert(field.name().to_string(), rendered);
                }
            }
        }

        let mut visitor = V {
            event: None,
            fields: BTreeMap::new(),
        };
        event.record(&mut visitor);
        if let Some(event) = visitor.event {
            self.events.lock().unwrap().push(CapturedTelemetryEvent {
                event,
                fields: visitor.fields,
            });
        }
    }
}

async fn register_invoked_skill(
    orch: &ConversationOrchestrator,
    name: &str,
    path: &std::path::Path,
    content: &str,
    agent_id: Option<&str>,
) {
    let session_id = orch.session.lock().await.session_id.to_string();
    compaction::invoked_skills::register_scoped(
        name,
        path,
        content,
        compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), agent_id),
    );
}

#[tokio::test]
async fn reread_restores_fresh_content_not_stale_snapshot() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("live.txt");
    // Snapshot recorded "OLD"; on disk the file now holds "NEW CONTENT".
    std::fs::write(&path, "NEW CONTENT").expect("write file");
    let map = tool_api::read_file_state::new_read_file_state_map();
    set(&map, path.clone(), stale_entry("OLD STALE SNAPSHOT"));

    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    let restored = orch.restore_post_compact_attachments().await;
    assert_eq!(
        restored.len(),
        2,
        "native file attachments render call and result separately"
    );
    assert_eq!(
        restored[0].text_content(),
        format!(
        "<system-reminder>\nCalled the Read tool with the following input: {}\n</system-reminder>",
        serde_json::json!({"file_path": path})
    )
    );
    let body = restored[1].text_content();
    assert_eq!(
        body,
        "<system-reminder>\nResult of calling the Read tool:\n1\tNEW CONTENT\n</system-reminder>"
    );
    assert!(
        body.contains("NEW CONTENT"),
        "must restore FRESH disk content; got: {body}"
    );
    assert!(
        !body.contains("OLD STALE SNAPSHOT"),
        "must NOT restore the stale snapshot content; got: {body}"
    );

    // Exactly one success event fired, no error event.
    assert_eq!(
        restore_names(&sink.events().await),
        vec!["tengu_post_compact_file_restore_success".to_string()]
    );
}

#[tokio::test]
async fn oversized_reread_uses_exact_compact_file_reference_attachment() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("large.txt");
    std::fs::write(&path, "x".repeat(20_002)).expect("write file");
    let map = tool_api::read_file_state::new_read_file_state_map();
    set(&map, path.clone(), stale_entry("stale"));

    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    let restored = orch.restore_post_compact_attachments().await;
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored[0].text_content(),
        format!(
            "<system-reminder>\nNote: {} was read before the last conversation was summarized, but the contents are too large to include. Use Read tool if you need to access it.\n</system-reminder>",
            path.display()
        )
    );
    assert!(
        restore_names(&sink.events().await).is_empty(),
        "the native compact-reference fallback returns before success telemetry"
    );
}

#[tokio::test]
async fn preserved_read_tool_use_prevents_file_restore() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("already.txt");
    std::fs::write(&path, "current").expect("write file");
    let map = tool_api::read_file_state::new_read_file_state_map();
    set(&map, path.clone(), stale_entry("stale"));
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;
    let boundary = protocol::ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolUse {
            id: protocol::ToolUseId::new(),
            name: "Read".to_string(),
            input: serde_json::json!({"file_path": path}),
            provider_id: None,
        }],
        stop_reason: None,
    };

    let restored = orch
        .restore_post_compact_attachments_against(&[boundary])
        .await;
    assert!(restored.is_empty());
    assert!(restore_names(&sink.events().await).is_empty());
}

#[tokio::test]
async fn textual_file_reference_does_not_suppress_a_fresh_restore() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("already<&>.txt");
    std::fs::write(&path, "current").expect("write file");
    let map = tool_api::read_file_state::new_read_file_state_map();
    set(&map, path.clone(), stale_entry("stale"));
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;
    let escaped = crate::prompt::sanitize::escape_reminder_path(&path.to_string_lossy());
    let boundary = protocol::ConversationMessage::user_meta(
        protocol::MessageId::new(),
        format!(
            "<system-reminder>\nNote: {escaped} was read before the last conversation was summarized, but the contents are too large to include. Use Read tool if you need to access it.\n</system-reminder>"
        ),
    );

    let restored = orch
        .restore_post_compact_attachments_against(&[boundary])
        .await;
    assert_eq!(
        restored.len(),
        2,
        "kXo dedups actual Read calls, not text that resembles an attachment"
    );
    assert_eq!(restore_names(&sink.events().await).len(), 1);
}

#[tokio::test]
async fn preserved_unchanged_read_stub_does_not_hide_the_missing_file_content() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("live.txt");
    std::fs::write(&path, "fresh").unwrap();
    let map = tool_api::read_file_state::new_read_file_state_map();
    set(&map, path.clone(), stale_entry("stale"));
    let orch = orch_with_bus(
        dir.path().to_path_buf(),
        map,
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    let id = protocol::ToolUseId::new();
    let call = protocol::ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        stop_reason: None,
        content: vec![protocol::ContentBlock::ToolUse {
            id: id.clone(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": path}),
            provider_id: Some("toolu-native-id".into()),
        }],
    };
    let result = protocol::ConversationMessage::User {
        id: protocol::MessageId::new(),
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: id,
            content: tool_file::read::FILE_UNCHANGED_SHORT.into(),
            is_error: false,
            provider_tool_use_id: Some("toolu-native-id".into()),
            content_blocks: None,
        }],
    };
    let restored = orch
        .restore_post_compact_attachments_against(&[call, result])
        .await;
    assert_eq!(restored.len(), 2);
    assert!(restored[1].text_content().contains("1\tfresh"));
}

#[tokio::test]
async fn file_restore_uses_utf16_rounding_and_dense_json_token_limits() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, content, reference) in [
        ("edge.txt", "x".repeat(20_001), false),
        ("cjk.txt", "你".repeat(20_001), false),
        ("dense.json", "x".repeat(10_001), true),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let map = tool_api::read_file_state::new_read_file_state_map();
        set(&map, path.clone(), stale_entry("stale"));
        let orch = orch_with_bus(
            dir.path().to_path_buf(),
            map.clone(),
            Arc::new(telemetry::InMemorySink::new()),
        )
        .await;
        let restored = orch.restore_post_compact_attachments().await;
        assert_eq!(restored.len(), if reference { 1 } else { 2 }, "{name}");
        if !reference {
            assert!(
                tool_api::read_file_state::get(&map, &path).is_some(),
                "a fresh restoration must repopulate Read state"
            );
        }
    }
}

#[tokio::test]
async fn file_restore_budgets_json_escapes_and_continues_after_overflow() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    for (name, content, timestamp) in [
        ("first.txt", "\u{0001}".repeat(20_000), 3),
        ("overflow.txt", "\u{0001}".repeat(20_000), 2),
        ("small.txt", "small".to_string(), 1),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let mut entry = stale_entry("stale");
        entry.mtime_ms = timestamp;
        set(&map, path, entry);
    }
    let orch = orch_with_bus(
        dir.path().to_path_buf(),
        map,
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    let restored = orch.restore_post_compact_attachments().await;
    assert_eq!(
        restored.len(),
        4,
        "30k + 30k serialized tokens overflow, but a later small file fits"
    );
    let all = restored
        .iter()
        .map(protocol::ConversationMessage::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("first.txt"));
    assert!(!all.contains("overflow.txt"));
    assert!(all.contains("small.txt"));
}

#[tokio::test]
async fn only_meta_text_bodies_suppress_skill_registry_truncation() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let first = "a".repeat(20_000);
    let second = "b".repeat(20_000);
    let body = format!("{first}\n\n{second}");
    for is_meta in [false, true] {
        let orch = orch_with_bus(
            dir.path().to_path_buf(),
            tool_api::read_file_state::new_read_file_state_map(),
            Arc::new(telemetry::InMemorySink::new()),
        )
        .await;
        register_invoked_skill(&orch, "large", std::path::Path::new("/skill"), &body, None).await;
        let message = protocol::ConversationMessage::User {
            id: protocol::MessageId::new(),
            is_meta,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            content: if is_meta {
                vec![
                    protocol::ContentBlock::Text {
                        text: first.clone(),
                    },
                    protocol::ContentBlock::Text {
                        text: second.clone(),
                    },
                ]
            } else {
                vec![protocol::ContentBlock::Text { text: body.clone() }]
            },
        };
        assert_eq!(
            orch.restore_post_compact_attachments_against(&[message])
                .await
                .len(),
            1
        );
        let session_id = orch.session.lock().await.session_id.to_string();
        let rows = compaction::invoked_skills::filter_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None),
        );
        assert_eq!(
            rows[0].content == body,
            is_meta,
            "only meta-user text joined with a blank line matches the native Lle body"
        );
    }
}

/// The plan file never comes back through the FILE-restore arm (`Ets` filters
/// it out of `yts`), because 2.1.266 re-attaches it through its own
/// `plan_file_reference` provider (`_ts`) instead — `KOe` composes
/// `[...files, ...skills, ...planFileReference, …]`.
#[tokio::test]
async fn plan_file_is_excluded_from_post_compact_restore() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map.clone(), sink.clone()).await;
    let session_id = orch.session.lock().await.session_id;
    let path = std::path::PathBuf::from(ConversationOrchestrator::plan_file_path(
        &session_id,
        dir.path(),
        Some("plans"),
    ));
    std::fs::create_dir_all(path.parent().expect("plans parent")).expect("create plans dir");
    std::fs::write(&path, "secret plan").expect("write plan");
    set(&map, path, stale_entry("stale plan"));

    let restored = orch.restore_post_compact_attachments().await;
    // No FILE restore happened — that is what the exclusion means, and the
    // absence of restore telemetry is the witness.
    assert!(restore_names(&sink.events().await).is_empty());
    // What DOES come back is the dedicated `plan_file_reference`, carrying the
    // plan's real contents rather than the stale read-file snapshot.
    assert_eq!(restored.len(), 1, "{restored:?}");
    let body = restored[0].text_content();
    assert!(
        body.contains("A plan file exists from plan mode at: "),
        "{body}"
    );
    assert!(body.contains("secret plan"), "{body}");
    assert!(
        !body.contains("stale plan"),
        "the stale read-file snapshot must not be what is restored: {body}"
    );
}

#[tokio::test]
async fn memory_entrypoints_are_excluded_but_nested_memory_files_can_restore() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    for name in [
        branding::MEMORY_FILE.to_string(),
        branding::MEMORY_LOCAL_FILE.to_string(),
        format!("nested/{}", branding::MEMORY_FILE),
    ] {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "instructions").unwrap();
        set(&map, path, stale_entry("stale"));
    }
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;
    let restored = orch.restore_post_compact_attachments().await;
    assert_eq!(restored.len(), 2);
    assert!(restored[0]
        .text_content()
        .contains(&format!("nested/{}", branding::MEMORY_FILE)));
    assert_eq!(restore_names(&sink.events().await).len(), 1);
}

#[tokio::test]
async fn deleted_file_is_dropped_and_fires_error_event() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    // A path recorded in the snapshot but never written to disk (deleted).
    let missing = dir.path().join("gone.txt");
    let map = tool_api::read_file_state::new_read_file_state_map();
    set(
        &map,
        missing,
        stale_entry("content the model saw before deletion"),
    );

    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    let restored = orch.restore_post_compact_attachments().await;
    assert!(
        restored.is_empty(),
        "an unreadable/deleted file must be dropped, not restored from the stale snapshot"
    );
    assert_eq!(
        restore_names(&sink.events().await),
        vec!["tengu_post_compact_file_restore_error".to_string()]
    );
}

#[tokio::test]
async fn host_seed_snapshot_is_not_restored_and_remains_cached() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("seeded.txt");
    std::fs::write(&path, "seeded on disk").expect("write file");
    let map = tool_api::read_file_state::new_read_file_state_map();
    tool_api::read_file_state::set_with_model_context(
        &map,
        path.clone(),
        stale_entry("host seed snapshot"),
        false,
    );

    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map.clone(), sink.clone()).await;

    let restored = orch.restore_post_compact_attachments().await;
    assert!(
        restored.is_empty(),
        "host-seeded snapshots must not be restored into model context"
    );
    assert!(restore_names(&sink.events().await).is_empty());
    assert!(
        tool_api::read_file_state::get(&map, &path).is_some(),
        "host-seeded snapshot must stay cached for staleness/dedup after compaction"
    );
}

#[tokio::test]
async fn cancelled_compact_keeps_model_visible_read_state_untouched() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("live.txt");
    std::fs::write(&path, "current").expect("write file");
    let map = tool_api::read_file_state::new_read_file_state_map();
    set(&map, path.clone(), stale_entry("model-visible snapshot"));
    let orch = orch_with_bus(
        dir.path().to_path_buf(),
        map.clone(),
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let result = compaction::IterationCompactionResult {
        messages: vec![ConversationMessage::user(
            MessageId::new(),
            "summary".to_string(),
        )],
        layers_applied: vec![compaction::CompactionLayer::Autocompact],
        total_tokens_freed: 1,
        cache_hit: false,
        consecutive_failures: 0,
        was_compacted: true,
        rapid_refill_breaker_tripped: false,
        consecutive_rapid_refills: 0,
        messages_to_preserve: Vec::new(),
        media_analysis_to_preserve: Vec::new(),
        compaction_usage: None,
        compaction_model: None,
        raw_summary_text: "test summary".to_string(),
    };

    let applied = orch
        .apply_post_compact(
            result,
            compaction::CompactTrigger::Manual,
            1,
            1,
            1,
            std::time::Instant::now(),
            Some(&cancel),
        )
        .await;

    assert!(applied.is_none());
    assert_eq!(
        tool_api::read_file_state::get(&map, &path)
            .expect("cancel must preserve read-state")
            .content,
        "model-visible snapshot"
    );
}

#[tokio::test]
async fn apply_post_compact_clears_cached_git_status() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let orch = orch_with_bus(
        dir.path().to_path_buf(),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    *orch.prompt_runtime.git_status_snapshot.lock().await = Some(GitStatusSnapshot {
        probe: None,
        block: Some("stale git status".to_string()),
    });
    let result = compaction::IterationCompactionResult {
        messages: vec![ConversationMessage::user(
            MessageId::new(),
            "summary".to_string(),
        )],
        layers_applied: vec![compaction::CompactionLayer::Autocompact],
        total_tokens_freed: 1,
        cache_hit: false,
        consecutive_failures: 0,
        was_compacted: true,
        rapid_refill_breaker_tripped: false,
        consecutive_rapid_refills: 0,
        messages_to_preserve: Vec::new(),
        media_analysis_to_preserve: Vec::new(),
        compaction_usage: None,
        compaction_model: None,
        raw_summary_text: "test summary".to_string(),
    };

    let applied = orch
        .apply_post_compact(
            result,
            compaction::CompactTrigger::Manual,
            1,
            1,
            1,
            std::time::Instant::now(),
            None,
        )
        .await;

    assert!(applied.is_some());
    assert!(
        orch.prompt_runtime
            .git_status_snapshot
            .lock()
            .await
            .is_none(),
        "compact must invalidate the frozen git-status cache"
    );
}

#[tokio::test]
async fn transcript_append_failure_emits_session_persistence_failed() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = TelemetryEventCapture::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(capture.clone()));
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(
        dir.path().to_path_buf(),
        tool_api::read_file_state::new_read_file_state_map(),
        sink.clone(),
    )
    .await;
    let session_id = orch.session.lock().await.session_id.to_string();
    let operation = "hook_attachment";
    let error = "write_fail".to_string();

    orch.record_transcript_append_failure(&session_id, operation, &error)
        .await;

    let events = capture.events.lock().unwrap().clone();
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event.event == telemetry::tengu::session::PERSISTENCE_FAILED)
        .cloned()
        .collect();
    assert_eq!(
        matching,
        vec![CapturedTelemetryEvent {
            event: telemetry::tengu::session::PERSISTENCE_FAILED.to_string(),
            fields: BTreeMap::new(),
        }],
        "append-failure telemetry must emit exactly one session-persistence event with no payload fields; all captured events: {events:?}",
    );
    assert!(
        sink.events().await.is_empty(),
        "session persistence failure should not be routed through AnalyticsBus",
    );
    let leaked = format!("{matching:?}");
    assert!(
        !leaked.contains(&session_id) && !leaked.contains(operation) && !leaked.contains(&error),
        "captured telemetry event must not leak raw session/operation/error details: {leaked}",
    );
}

#[tokio::test]
async fn success_and_error_events_fire_per_file() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let live = dir.path().join("a.txt");
    std::fs::write(&live, "alive").expect("write file");
    let gone = dir.path().join("b.txt"); // never created

    let map = tool_api::read_file_state::new_read_file_state_map();
    // Higher mtime → selected/re-read first (DESC), but ordering of the two
    // telemetry events is not asserted — only the multiset.
    set(&map, live.clone(), stale_entry("stale-a"));
    set(&map, gone, stale_entry("stale-b"));

    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    let restored = orch.restore_post_compact_attachments().await;
    assert_eq!(
        restored.len(),
        2,
        "only the live file's call and result survive"
    );
    assert!(restored[1].text_content().contains("alive"));

    let mut names = restore_names(&sink.events().await);
    names.sort();
    assert_eq!(
        names,
        vec![
            "tengu_post_compact_file_restore_error".to_string(),
            "tengu_post_compact_file_restore_success".to_string(),
        ]
    );
}

#[tokio::test]
async fn empty_read_state_restores_nothing_and_fires_no_events() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    let restored = orch.restore_post_compact_attachments().await;
    assert!(restored.is_empty());
    assert!(restore_names(&sink.events().await).is_empty());
}

// ── P2-12: post-compact SKILL restoration (`rRg`) ─────────────────────────

#[tokio::test]
async fn invoked_skill_reappears_as_attachment_post_compact() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    // No files read this turn — the skill arm must still run.
    let map = tool_api::read_file_state::new_read_file_state_map();
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    // A skill was invoked before the compaction (main thread → agentId None).
    register_invoked_skill(
        &orch,
        "deploy",
        std::path::Path::new("/skills/deploy"),
        "Deploy guidelines: run the pipeline.",
        None,
    )
    .await;

    let restored = orch.restore_post_compact_attachments().await;
    assert_eq!(restored.len(), 1, "one invoked_skills meta message");
    let body = restored[0].text_content();
    assert!(
        body.contains("The following skills were invoked EARLIER in this session"),
        "byte-faithful invoked_skills preamble; got: {body}"
    );
    assert!(body.contains("### Skill: deploy"));
    assert!(body.contains("Path: /skills/deploy"));
    assert!(body.contains("Deploy guidelines: run the pipeline."));

    // The registry SURVIVES compaction (not cleared) — a second restore still
    // sees the skill (documented no-clear rationale).
    let again = orch.restore_post_compact_attachments().await;
    assert_eq!(again.len(), 1, "registry survives compaction");
    assert!(again[0].text_content().contains("### Skill: deploy"));
}

#[tokio::test]
async fn invoked_skill_split_surrogate_reaches_attachment_with_exact_utf16_sidecar() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let orch = orch_with_bus(
        dir.path().to_path_buf(),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    let body = format!("{}😀{}", "x".repeat(19_899), "y".repeat(200));
    register_invoked_skill(
        &orch,
        "utf16",
        std::path::Path::new("/skills/utf16"),
        &body,
        None,
    )
    .await;

    let restored = orch.restore_post_compact_attachments().await;

    assert_eq!(restored.len(), 1);
    let protocol::ConversationMessage::User { content, .. } = &restored[0] else {
        panic!("post-compact attachment must be a user message");
    };
    let protocol::ContentBlock::TextJsUtf16 {
        utf16_code_units, ..
    } = &content[0]
    else {
        panic!("split surrogate attachment must retain its UTF-16 sidecar");
    };
    assert!(
        utf16_code_units
            .windows(2)
            .any(|window| window == [0xD83D, 0x000A]),
        "exact attachment must retain the high surrogate before the truncation marker"
    );
}

#[tokio::test]
async fn invoked_skill_never_restores_into_another_session() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = orch_with_bus(
        dir.path().to_path_buf(),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    let other = orch_with_bus(
        dir.path().to_path_buf(),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    assert_ne!(
        source.session.lock().await.session_id,
        other.session.lock().await.session_id
    );
    register_invoked_skill(
        &source,
        "deploy",
        std::path::Path::new("/skills/deploy"),
        "source-only body",
        None,
    )
    .await;

    assert!(other.restore_post_compact_attachments().await.is_empty());
    let restored = source.restore_post_compact_attachments().await;
    assert_eq!(restored.len(), 1);
    assert!(restored[0].text_content().contains("source-only body"));
}

#[tokio::test]
async fn invoked_skill_already_in_preserved_attachment_is_not_duplicated() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink).await;

    let content = "Deploy guidelines: run the pipeline.";
    register_invoked_skill(
        &orch,
        "deploy",
        std::path::Path::new("/skills/deploy"),
        content,
        None,
    )
    .await;
    let preserved = orch.restore_post_compact_attachments().await;
    assert_eq!(preserved.len(), 1, "first compaction restores the skill");

    let restored = orch
        .restore_post_compact_attachments_against(&preserved)
        .await;
    assert!(
        restored.is_empty(),
        "preserved invoked-skills content must not be emitted twice"
    );
}

#[tokio::test]
async fn invoked_skill_with_markdown_separator_is_deduped_without_parsing_its_content() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink).await;

    let content = "Deploy the first stage.\n\n---\n\nThen deploy the second stage.";
    register_invoked_skill(
        &orch,
        "deploy",
        std::path::Path::new("/skills/deploy"),
        content,
        None,
    )
    .await;

    let first = orch.restore_post_compact_attachments().await;
    assert_eq!(first.len(), 1, "first compaction restores the skill");

    let persisted = orch.to_jsonl_message(&first[0], "session", None, None, None, None);
    let persisted: session::JsonlMessage = serde_json::from_str(
        &serde_json::to_string(&persisted).expect("serialize attachment metadata"),
    )
    .expect("round-trip attachment metadata");
    assert_eq!(
        persisted
            .extra
            .get("invokedSkillContents")
            .and_then(serde_json::Value::as_array)
            .and_then(|contents| contents.first())
            .and_then(serde_json::Value::as_str),
        Some(content),
        "the opaque body must be persisted without delimiter parsing"
    );

    let resumed = orch_with_bus(
        dir.path().to_path_buf(),
        tool_api::read_file_state::new_read_file_state_map(),
        Arc::new(telemetry::InMemorySink::new()),
    )
    .await;
    resumed
        .restore_resume_runtime_metadata(std::slice::from_ref(&persisted))
        .await;
    let replayed =
        crate::state_from_messages(uuid::Uuid::new_v4(), std::slice::from_ref(&persisted));
    let restored = resumed
        .restore_post_compact_attachments_against(&replayed.history)
        .await;
    assert!(
        restored.is_empty(),
        "resume followed by compact must preserve structural dedup even when Markdown contains the renderer separator"
    );
}

#[tokio::test]
async fn no_invoked_skills_restores_no_skill_message() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    // Empty registry → no skill attachment (and no file snapshot → nothing).
    let restored = orch.restore_post_compact_attachments().await;
    assert!(restored.is_empty());
}

#[tokio::test]
async fn subagent_skill_not_restored_on_main_thread() {
    let _rg = registry_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let map = tool_api::read_file_state::new_read_file_state_map();
    let sink = Arc::new(telemetry::InMemorySink::new());
    let orch = orch_with_bus(dir.path().to_path_buf(), map, sink.clone()).await;

    // A skill invoked under a subagent (agentId Some) must NOT surface on the
    // main-thread (agentId None) restore — `kGo` filters by agentId.
    register_invoked_skill(
        &orch,
        "child-skill",
        std::path::Path::new("/skills/child"),
        "child body",
        Some("agent:child"),
    )
    .await;

    let restored = orch.restore_post_compact_attachments().await;
    assert!(
        restored.is_empty(),
        "subagent skill must not restore on the main thread"
    );
}
