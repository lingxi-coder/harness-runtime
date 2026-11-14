//! Public Read invocations exercise exact-count admission, fallback and ownership.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tool_api::read_context_room::{ReadContextRoom, ReadTokenCounter};
use tool_api::tool_trait::{Tool, ToolError};
use tool_file::FileReadTool;

struct Counter {
    outcome: Result<Option<u64>, String>,
    observed: Mutex<Vec<Vec<u16>>>,
}

#[async_trait::async_trait]
impl ReadTokenCounter for Counter {
    async fn count_read_tokens(
        &self,
        content: &Utf16JsonProjection,
    ) -> Result<Option<u64>, String> {
        self.observed
            .lock()
            .unwrap()
            .push(content.string_units("").unwrap());
        self.outcome.clone()
    }
}

fn fixture() -> (TempDir, std::path::PathBuf, Vec<u16>) {
    let directory = TempDir::new().unwrap();
    let target = directory.path().join("source.txt");
    // The local estimate exceeds the default25k budget, within the ordinary
    // byte cap. Exact counting must receive the unpaired source unit intact.
    let mut units = vec![b'x' as u16; 120_000];
    units.push(0xd800);
    let bytes: Vec<u8> = [0xfeff]
        .into_iter()
        .chain(units.iter().copied())
        .flat_map(u16::to_le_bytes)
        .collect();
    std::fs::write(&target, bytes).unwrap();
    (directory, target, units)
}

fn tool(directory: &TempDir) -> (FileReadTool, tool_api::read_file_state::ReadFileStateMap) {
    let context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![directory.path().to_path_buf()],
    );
    let state = context.read_file_state.clone();
    (FileReadTool::new(context), state)
}

fn input(path: &std::path::Path, allow_large: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&serde_json::json!({
        "file_path": path, "allow_large": allow_large,
    }))
    .unwrap()
}

#[tokio::test]
async fn exact_counts_override_estimates_and_failed_counts_use_the_native_fallback() {
    for (outcome, partial) in [
        (Ok(Some(0)), false),
        (Ok(Some(1_000)), false),
        (Ok(Some(25_000)), false),
        (Ok(Some(100_000)), true),
        (Ok(None), true),
        (Err("counter unavailable".into()), true),
    ] {
        let (directory, target, units) = fixture();
        let (tool, state) = tool(&directory);
        let counter = Arc::new(Counter {
            outcome,
            observed: Mutex::new(Vec::new()),
        });
        let room = Arc::new(ReadContextRoom::new(60_000, 0).with_token_counter(counter.clone()));
        let mut call = tool_api::test_support::fresh_ctx();
        call.options.read_context_room = Some(room.clone());
        let result = tool
            .call(
                input(&target, false),
                call,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            counter.observed.lock().unwrap().as_slice(),
            std::slice::from_ref(&units)
        );
        assert_eq!(
            result
                .data
                .at_pointer("/file/truncatedByTokenCap")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            partial
        );
        assert_eq!(
            tool_api::read_file_state::get_metadata(&state, &target)
                .unwrap()
                .is_partial_view,
            partial
        );
        if !partial {
            assert_eq!(result.data.string_units("/file/content"), Some(units));
        }
        assert_eq!(
            room.claimed(),
            0,
            "ordinary native reads do not claim lifted room"
        );
    }
}

#[tokio::test]
async fn counter_only_reads_count_exact_source_without_large_allowance_or_claims() {
    for allow_large in [false, true] {
        let (directory, target, units) = fixture();
        let (tool, _) = tool(&directory);
        let counter = Arc::new(Counter {
            outcome: Ok(Some(2500)),
            observed: Mutex::new(Vec::new()),
        });
        let room = Arc::new(ReadContextRoom::counter_only().with_token_counter(counter.clone()));
        let mut call = tool_api::test_support::fresh_ctx();
        call.options.read_context_room = Some(room.clone());
        let result = tool
            .call(
                input(&target, allow_large),
                call,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            counter.observed.lock().unwrap().as_slice(),
            std::slice::from_ref(&units)
        );
        assert_eq!(result.data.string_units("/file/content"), Some(units));
        assert!(!room.has_context_room());
        assert_eq!(room.room_tokens(0), 0);
        assert_eq!(room.claimed(), 0);
    }
}

#[tokio::test]
async fn counter_only_preserves_unavailable_guidance_and_the_normal_byte_cap() {
    let (directory, _, _) = fixture();
    let target = directory.path().join("over-byte-cap.txt");
    std::fs::write(&target, "x".repeat(300_000)).unwrap();
    let (tool, _) = tool(&directory);
    let counter = Arc::new(Counter {
        outcome: Ok(Some(0)),
        observed: Mutex::new(Vec::new()),
    });
    let room = Arc::new(ReadContextRoom::counter_only().with_token_counter(counter.clone()));
    for allow_large in [false, true] {
        let absent = tool
            .call(
                input(&target, allow_large),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap_err()
            .model_facing_message();
        let mut call = tool_api::test_support::fresh_ctx();
        call.options.read_context_room = Some(room.clone());
        let counter_only = tool
            .call(
                input(&target, allow_large),
                call,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap_err()
            .model_facing_message();
        assert_eq!(counter_only, absent);
        if allow_large {
            assert!(counter_only.string_at("").unwrap().ends_with(
                " allow_large is not available where this read runs; read it in parts with offset and limit."
            ));
        }
    }
    assert!(counter.observed.lock().unwrap().is_empty());
    assert_eq!(room.claimed(), 0);
}

#[tokio::test]
async fn lifted_read_counts_numbered_exact_text_and_claims_the_provider_count() {
    for count in [0, 1_000] {
        let (directory, target, units) = fixture();
        let (tool, _) = tool(&directory);
        let counter = Arc::new(Counter {
            outcome: Ok(Some(count)),
            observed: Mutex::new(Vec::new()),
        });
        let room = Arc::new(ReadContextRoom::new(60_000, 0).with_token_counter(counter.clone()));
        let mut call = tool_api::test_support::fresh_ctx();
        call.options.read_context_room = Some(room.clone());
        let result = tool
            .call(
                input(&target, true),
                call,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let mut numbered = "1\t".encode_utf16().collect::<Vec<_>>();
        numbered.extend_from_slice(&units);
        assert_eq!(counter.observed.lock().unwrap().as_slice(), &[numbered]);
        assert_eq!(result.data.string_units("/file/content"), Some(units));
        assert_eq!(room.claimed(), count);
    }
}

#[tokio::test]
async fn unrelated_routes_and_small_reads_do_not_start_provider_counting() {
    let (directory, target, _) = fixture();
    let (tool, _) = tool(&directory);
    let counter = Arc::new(Counter {
        outcome: Ok(Some(0)),
        observed: Mutex::new(Vec::new()),
    });
    let mismatched = Arc::new(
        ReadContextRoom::new(60_000, 0)
            .with_route("another-model".into(), None)
            .with_token_counter(counter.clone()),
    );
    let mut call = tool_api::test_support::fresh_ctx();
    call.options.read_context_room = Some(mismatched);
    let result = tool
        .call(
            input(&target, false),
            call,
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(
        result
            .data
            .at_pointer("/file/truncatedByTokenCap")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(counter.observed.lock().unwrap().is_empty());

    std::fs::write(&target, "small file").unwrap();
    let mut call = tool_api::test_support::fresh_ctx();
    call.options.read_context_room = Some(Arc::new(
        ReadContextRoom::new(60_000, 0).with_token_counter(counter.clone()),
    ));
    let result = tool
        .call(
            input(&target, false),
            call,
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(result.data.string_at("/file/content"), Some("small file"));
    assert!(counter.observed.lock().unwrap().is_empty());
}

struct PendingCounter {
    entered: tokio::sync::Notify,
    dropped: Arc<AtomicBool>,
    calls: AtomicUsize,
}

struct CountFutureGuard(Arc<AtomicBool>);
impl Drop for CountFutureGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl ReadTokenCounter for PendingCounter {
    async fn count_read_tokens(
        &self,
        _content: &Utf16JsonProjection,
    ) -> Result<Option<u64>, String> {
        let _guard = CountFutureGuard(self.dropped.clone());
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_or_dropped_read_owns_the_provider_future_and_never_publishes_state() {
    for drop_invocation in [false, true] {
        let (directory, target, _) = fixture();
        let (tool, state) = tool(&directory);
        let counter = Arc::new(PendingCounter {
            entered: tokio::sync::Notify::new(),
            dropped: Arc::new(AtomicBool::new(false)),
            calls: AtomicUsize::new(0),
        });
        let room = Arc::new(ReadContextRoom::new(60_000, 0).with_token_counter(counter.clone()));
        let cancellation = CancellationToken::new();
        let mut call = tool_api::test_support::fresh_ctx();
        call.options.read_context_room = Some(room.clone());
        call.cancel = Some(cancellation.clone());
        let arguments = input(&target, false);
        let invocation = tokio::spawn(async move {
            tool.call(arguments, call, tool_api::test_support::fresh_tx())
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), counter.entered.notified())
            .await
            .unwrap();
        if drop_invocation {
            invocation.abort();
            assert!(invocation.await.unwrap_err().is_cancelled());
        } else {
            cancellation.cancel();
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(3), invocation)
                    .await
                    .unwrap()
                    .unwrap(),
                Err(ToolError::Aborted)
            ));
        }
        assert!(counter.dropped.load(Ordering::Acquire));
        assert_eq!(counter.calls.load(Ordering::Relaxed), 1);
        assert!(tool_api::read_file_state::get_metadata(&state, &target).is_none());
        assert_eq!(room.claimed(), 0);
    }
}

#[tokio::test]
async fn direct_read_range_arguments_reject_unsafe_integers_before_io() {
    let directory = TempDir::new().unwrap();
    let (tool, _) = tool(&directory);
    for (name, value) in [
        ("offset", "9007199254740992"),
        ("limit", "9007199254740992"),
        ("offset", "-1"),
        ("offset", "1.5"),
        ("limit", "0"),
        ("limit", "null"),
        ("offset", "true"),
    ] {
        let input = Utf16JsonProjection::parse(&format!(
            r#"{{"file_path":"missing.txt","{name}":{value}}}"#
        ))
        .unwrap();
        let result = tool
            .call(
                input,
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "{name}={value}"
        );
    }
}
