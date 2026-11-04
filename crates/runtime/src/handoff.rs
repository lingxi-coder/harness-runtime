//! Per-owner, idempotent handoff operations. Cancelling a summary never cancels
//! the source turn, and constructing a summary never constructs a target Agent.

use client::protocol::{events::ClientEvent, listings::SessionModeDto, text::ClientTextDto};
use lingxi_core::host::{session_context::ConversationMode, OrchestratorHandle};
use lingxi_core::types::SessionId;
use orchestrator::{session_handoff::SessionHandoffCapture, ConversationOrchestrator};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_ACTIVE_OPERATIONS: usize = 16;
const MAX_COMPLETED_OPERATIONS: usize = 64;
const MAX_RETRY_CAPTURES: usize = 8;
const RETRY_RETENTION: Duration = Duration::from_secs(5 * 60);

struct RetryCapture {
    capture: Arc<SessionHandoffCapture>,
    expires_at: Instant,
    expiry_cancel: CancellationToken,
}

impl Drop for RetryCapture {
    fn drop(&mut self) {
        self.expiry_cancel.cancel();
    }
}

struct Operation {
    source_session_id: String,
    target_mode: SessionModeDto,
    cancel: CancellationToken,
    event: Option<ClientEvent>,
    retry: Option<RetryCapture>,
}

#[derive(Default)]
struct Operations {
    by_id: HashMap<String, Operation>,
    completed: VecDeque<String>,
}

impl Operations {
    fn active_count(&self) -> usize {
        self.by_id.len() - self.completed.len()
    }

    fn finish(
        &mut self,
        request_id: &str,
        event: ClientEvent,
        capture: Option<Arc<SessionHandoffCapture>>,
    ) -> Option<(ClientEvent, Option<(Instant, CancellationToken)>)> {
        let operation = self.by_id.get_mut(request_id)?;
        // This mutex serializes cancellation with terminal publication.
        let event = if operation.cancel.is_cancelled() {
            ClientEvent::SessionHandoffFailed {
                request_id: request_id.to_owned(),
                message: "handoff cancelled".into(),
            }
        } else {
            event
        };
        let retry = if !operation.cancel.is_cancelled()
            && matches!(event, ClientEvent::SessionHandoffFailed { .. })
        {
            capture.map(|capture| RetryCapture {
                capture,
                expires_at: Instant::now() + RETRY_RETENTION,
                expiry_cancel: CancellationToken::new(),
            })
        } else {
            None
        };
        let deadline = retry
            .as_ref()
            .map(|retry| (retry.expires_at, retry.expiry_cancel.clone()));
        operation.retry = retry;
        operation.event = Some(event.clone());
        self.completed.push_back(request_id.to_owned());
        while self.completed.len() > MAX_COMPLETED_OPERATIONS {
            if let Some(oldest) = self.completed.pop_front() {
                self.by_id.remove(&oldest);
            }
        }
        let mut retained = self
            .by_id
            .values()
            .filter(|operation| operation.retry.is_some())
            .count();
        for id in &self.completed {
            if retained <= MAX_RETRY_CAPTURES {
                break;
            }
            if self
                .by_id
                .get_mut(id)
                .is_some_and(|operation| operation.retry.take().is_some())
            {
                retained -= 1;
            }
        }
        Some((event, deadline))
    }

    fn expire_retry(&mut self, request_id: &str, now: Instant) {
        if let Some(operation) = self.by_id.get_mut(request_id) {
            if operation
                .retry
                .as_ref()
                .is_some_and(|retry| retry.expires_at <= now)
            {
                operation.retry = None;
            }
        }
    }
}

/// Keep one instance alongside the source runtime/connection, not alongside the
/// currently visible composer. IDs must be unique. The latest 64 terminal events
/// are replayable; old IDs are evicted and must never be reused by the client.
/// Failed model calls retain at most eight fixed captures for five minutes.
#[derive(Default)]
pub struct SessionHandoffManager {
    operations: Mutex<Operations>,
}

impl SessionHandoffManager {
    /// Start a capture once. Duplicate prepares replay or join without a new call.
    pub async fn prepare<F, Fut>(
        self: &Arc<Self>,
        orchestrator: Arc<ConversationOrchestrator>,
        request_id: String,
        target_mode: SessionModeDto,
        emit: F,
    ) -> Result<(), String>
    where
        F: Fn(ClientEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if request_id.is_empty() || request_id.len() > 128 {
            return Err("invalid handoff operation ID".into());
        }
        let source = orchestrator.current_session_id().await;
        let source_session_id = source.as_uuid().to_string();
        let cancel = CancellationToken::new();
        let cached = {
            let mut operations = self
                .operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(operation) = operations.by_id.get(&request_id) {
                if operation.source_session_id != source_session_id
                    || operation.target_mode != target_mode
                {
                    return Err("handoff operation belongs to another source or target".into());
                }
                match &operation.event {
                    Some(event) => Some(event.clone()),
                    None => return Ok(()),
                }
            } else {
                if operations.active_count() >= MAX_ACTIVE_OPERATIONS {
                    return Err("too many active handoff operations".into());
                }
                operations.by_id.insert(
                    request_id.clone(),
                    Operation {
                        source_session_id,
                        target_mode,
                        cancel: cancel.clone(),
                        event: None,
                        retry: None,
                    },
                );
                None
            }
        };
        if let Some(event) = cached {
            emit(event).await;
            return Ok(());
        }
        self.spawn_summary(
            orchestrator,
            source,
            request_id,
            target_mode,
            cancel,
            None,
            emit,
        );
        Ok(())
    }

    /// Explicitly retry a failed model call using exactly its original capture.
    /// Expired, evicted, cancelled, or capture-stage failures cannot be retried.
    pub async fn retry<F, Fut>(
        self: &Arc<Self>,
        orchestrator: Arc<ConversationOrchestrator>,
        request_id: String,
        emit: F,
    ) -> Result<(), String>
    where
        F: Fn(ClientEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let source = orchestrator.current_session_id().await;
        let cancel = CancellationToken::new();
        let (target_mode, capture) = {
            let mut operations = self
                .operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            operations.expire_retry(&request_id, Instant::now());
            let operation = operations
                .by_id
                .get(&request_id)
                .ok_or_else(|| "handoff retry is no longer available".to_string())?;
            if operation.source_session_id != source.as_uuid().to_string() {
                return Err("handoff operation belongs to another source".into());
            }
            if operation.event.is_none() {
                return Ok(());
            }
            if operations.active_count() >= MAX_ACTIVE_OPERATIONS {
                return Err("too many active handoff operations".into());
            }
            let operation = operations
                .by_id
                .get_mut(&request_id)
                .expect("checked operation");
            let capture = operation
                .retry
                .take()
                .ok_or_else(|| "handoff retry is no longer available".to_string())?
                .capture
                .clone();
            operation.event = None;
            operation.cancel = cancel.clone();
            let target_mode = operation.target_mode;
            operations.completed.retain(|id| id != &request_id);
            (target_mode, capture)
        };
        self.spawn_summary(
            orchestrator,
            source,
            request_id,
            target_mode,
            cancel,
            Some(capture),
            emit,
        );
        Ok(())
    }

    fn spawn_summary<F, Fut>(
        self: &Arc<Self>,
        orchestrator: Arc<ConversationOrchestrator>,
        source: SessionId,
        request_id: String,
        target_mode: SessionModeDto,
        cancel: CancellationToken,
        existing_capture: Option<Arc<SessionHandoffCapture>>,
        emit: F,
    ) where
        F: Fn(ClientEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let owner = Arc::downgrade(self);
        tokio::spawn(lingxi_core::host::session_flags::scope_request_session_id(
            source,
            async move {
                let target = match target_mode {
                    SessionModeDto::Chat => ConversationMode::Chat,
                    SessionModeDto::Code => ConversationMode::Code,
                };
                let capture = if let Some(capture) = existing_capture {
                    Ok(capture)
                } else {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => Err("handoff cancelled".to_string()),
                        result = orchestrator.capture_session_handoff(source, target) => result.map(Arc::new).map_err(|error| error.to_string()),
                    }
                };
                let result = match &capture {
                    Ok(capture) => orchestrator
                        .summarize_session_handoff(capture, cancel.clone())
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.clone()),
                };
                let event = match result {
                    Ok(summary) => ClientEvent::SessionHandoffPrepared {
                        request_id: request_id.clone(),
                        source_session_id: summary.source_session_id,
                        source_mode: mode_dto(summary.source_mode),
                        target_mode: mode_dto(summary.target_mode),
                        cutoff: summary.cutoff,
                        summary: ClientTextDto::from_code_units(
                            summary.summary.string_units("").unwrap_or_default(),
                        ),
                    },
                    Err(message) => ClientEvent::SessionHandoffFailed {
                        request_id: request_id.clone(),
                        message,
                    },
                };
                let Some(owner) = owner.upgrade() else {
                    return;
                };
                let (event, deadline) = {
                    let mut operations = owner
                        .operations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(result) = operations.finish(&request_id, event, capture.ok()) else {
                        return;
                    };
                    result
                };
                if let Some((deadline, expiry_cancel)) = deadline {
                    let weak_owner = Arc::downgrade(&owner);
                    let id = request_id.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            () = expiry_cancel.cancelled() => return,
                            () = tokio::time::sleep_until(deadline) => {}
                        }
                        if let Some(owner) = weak_owner.upgrade() {
                            owner
                                .operations
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .expire_retry(&id, Instant::now());
                        }
                    });
                }
                drop(owner);
                emit(event).await;
            },
        ));
    }

    /// Cancel only this read-only summary or discard a retained failed capture.
    pub fn cancel(&self, request_id: &str) -> bool {
        let mut operations = self
            .operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(operation) = operations.by_id.get_mut(request_id) else {
            return false;
        };
        if operation.event.is_some() {
            if operation.retry.take().is_some() {
                operation.event = Some(ClientEvent::SessionHandoffFailed {
                    request_id: request_id.to_owned(),
                    message: "handoff cancelled".into(),
                });
                return true;
            }
            return false;
        }
        operation.cancel.cancel();
        true
    }
}

impl Drop for SessionHandoffManager {
    fn drop(&mut self) {
        for operation in self
            .operations
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_id
            .values()
        {
            operation.cancel.cancel();
        }
    }
}

fn mode_dto(mode: ConversationMode) -> SessionModeDto {
    match mode {
        ConversationMode::Chat => SessionModeDto::Chat,
        ConversationMode::Code => SessionModeDto::Code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct FailOnce {
        calls: AtomicUsize,
        requests: Mutex<Vec<sidequery::SideQueryRequest>>,
    }

    #[async_trait::async_trait]
    impl sidequery::SideQueryClient for FailOnce {
        async fn query(
            &self,
            request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            self.requests.lock().unwrap().push(request);
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(sidequery::SideQueryError::InvalidResponse(
                    "temporary failure".into(),
                ));
            }
            let units = vec![b'x' as u16, 0xd800];
            Ok(sidequery::SideQueryResponse {
                text: Some(
                    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                        String::from_utf16_lossy(&units),
                        units,
                    )
                    .unwrap(),
                ),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    #[tokio::test]
    async fn explicit_retry_uses_the_original_capture_and_prepare_never_retries() {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let first = uuid::Uuid::new_v4();
        let source = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"fixed-session-mode","sessionId":id.as_uuid(),"sessionMode":"code","instructionEpoch":0}),
            serde_json::json!({"type":"user","uuid":first,"sessionId":id.as_uuid(),"parentUuid":null,"message":{"role":"user","content":"before cutoff"}})
        );
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        std::fs::write(&path, &source).unwrap();
        let writer = Arc::new(session::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let client = Arc::new(FailOnce {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "model".into()),
        );
        let orch = Arc::new(
            ConversationOrchestrator::new(
                orchestrator::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_session_id(id)
            .with_jsonl_writer(writer)
            .with_recap_runner(runner),
        );
        let manager = Arc::new(SessionHandoffManager::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emit = move |event| {
            let tx = tx.clone();
            async move {
                tx.send(event).unwrap();
            }
        };
        manager
            .prepare(
                orch.clone(),
                "same-id".into(),
                SessionModeDto::Chat,
                emit.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            ClientEvent::SessionHandoffFailed { .. }
        ));
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        manager
            .prepare(
                orch.clone(),
                "same-id".into(),
                SessionModeDto::Chat,
                emit.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(
            rx.recv().await.unwrap(),
            ClientEvent::SessionHandoffFailed { .. }
        ));
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        use std::io::Write;
        let later = serde_json::json!({"type":"user","uuid":uuid::Uuid::new_v4(),"sessionId":id.as_uuid(),"parentUuid":first,"message":{"role":"user","content":"after cutoff"}});
        writeln!(
            std::fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{later}"
        )
        .unwrap();
        manager
            .retry(orch.clone(), "same-id".into(), emit.clone())
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let ClientEvent::SessionHandoffPrepared {
            cutoff, summary, ..
        } = event
        else {
            panic!("retry should succeed")
        };
        assert_eq!(summary.code_units, vec![b'x' as u16, 0xd800]);
        assert!(cutoff.ends_with(&format!(":{}", source.len())));
        assert_eq!(client.calls.load(Ordering::SeqCst), 2);
        let requests = client.requests.lock().unwrap();
        let first_messages = &requests[0].messages;
        let second_messages = &requests[1].messages;
        assert!(first_messages.len() > 1 && second_messages.len() > 1);
        assert_eq!(
            &first_messages[..first_messages.len() - 1],
            &second_messages[..second_messages.len() - 1]
        );
        assert!(!format!("{:?}", requests[1].messages).contains("after cutoff"));
        drop(requests);
        let operations = manager.operations.lock().unwrap();
        assert!(operations.by_id["same-id"].retry.is_none());
        drop(operations);
        assert!(manager
            .retry(orch.clone(), "same-id".into(), emit)
            .await
            .is_err());

        // Failed captures are bounded independently of replayable events, and
        // eviction/expiry releases both the payload and its timer task.
        let capture = Arc::new(
            orch.capture_session_handoff(id, ConversationMode::Chat)
                .await
                .unwrap(),
        );
        let mut retained = Operations::default();
        let mut first_timer = None;
        for index in 0..=MAX_RETRY_CAPTURES {
            let request_id = format!("failure-{index}");
            retained.by_id.insert(
                request_id.clone(),
                Operation {
                    source_session_id: id.as_uuid().to_string(),
                    target_mode: SessionModeDto::Chat,
                    cancel: CancellationToken::new(),
                    event: None,
                    retry: None,
                },
            );
            let (_, deadline) = retained
                .finish(
                    &request_id,
                    ClientEvent::SessionHandoffFailed {
                        request_id: request_id.clone(),
                        message: "failed".into(),
                    },
                    Some(capture.clone()),
                )
                .unwrap();
            if index == 0 {
                first_timer = deadline.map(|(_, cancel)| cancel);
            }
        }
        assert!(retained.by_id["failure-0"].retry.is_none());
        assert!(first_timer.unwrap().is_cancelled());
        assert_eq!(
            retained
                .by_id
                .values()
                .filter(|op| op.retry.is_some())
                .count(),
            MAX_RETRY_CAPTURES
        );
        let last = format!("failure-{MAX_RETRY_CAPTURES}");
        let timer = retained.by_id[&last]
            .retry
            .as_ref()
            .unwrap()
            .expiry_cancel
            .clone();
        retained.expire_retry(&last, Instant::now() + RETRY_RETENTION);
        assert!(retained.by_id[&last].retry.is_none());
        assert!(timer.is_cancelled());
    }

    #[tokio::test]
    async fn terminal_replay_window_evicts_old_events_without_blocking_new_operations() {
        let mut operations = Operations::default();
        for index in 0..=MAX_COMPLETED_OPERATIONS {
            let id = format!("op-{index}");
            operations.by_id.insert(
                id.clone(),
                Operation {
                    source_session_id: "source".into(),
                    target_mode: SessionModeDto::Chat,
                    cancel: CancellationToken::new(),
                    event: None,
                    retry: None,
                },
            );
            let event = ClientEvent::SessionHandoffFailed {
                request_id: id.clone(),
                message: "failed".into(),
            };
            operations.finish(&id, event, None).unwrap();
        }
        assert_eq!(operations.active_count(), 0);
        assert_eq!(operations.by_id.len(), MAX_COMPLETED_OPERATIONS);
        assert!(!operations.by_id.contains_key("op-0"));
        assert!(operations
            .by_id
            .contains_key(&format!("op-{MAX_COMPLETED_OPERATIONS}")));
    }
}
