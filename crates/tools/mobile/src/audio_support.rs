//! Shared device-audio helpers for the public speech and voice tools.

use lingxi_core::host::audio::{
    AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioOperation, AudioOperationContext,
    AudioOperationId, AudioOperationKind, AudioOperationSuccess, AudioService,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tool_api::context::ToolUseContext;
use tool_api::tool_trait::ToolError;

static NEXT_AUDIO_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn operation_supported(
    capabilities: &AudioCapabilitySnapshot,
    operation: AudioOperationKind,
) -> bool {
    capabilities.supported_operations.contains(&operation)
}

pub(crate) async fn operation_context(
    service: &Arc<dyn AudioService>,
    use_context: &ToolUseContext,
    timeout: Duration,
) -> Result<AudioOperationContext, ToolError> {
    let capabilities = service.capabilities();
    let generation = NEXT_AUDIO_GENERATION.fetch_add(1, Ordering::Relaxed).max(1);
    let identity = AudioOperationId::new(generation, capabilities.service_epoch);
    let timeout_budget_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    use_context
        .audio_operation_context(
            identity,
            Some(timeout_budget_ms),
            capabilities.max_payload_bytes,
        )
        .await
        .map_err(map_audio_error)
}

/// Execute a short operation with both the host's cancellation token and a
/// monotonic deadline. Dropping the execute future invokes the bridge/native
/// adapter's targeted cancellation guard. The successful `StartRecording`
/// path drops its guard only after the handle has reached this host.
pub(crate) async fn execute_operation(
    service: &Arc<dyn AudioService>,
    use_context: &ToolUseContext,
    context: AudioOperationContext,
    operation: AudioOperation,
) -> Result<AudioOperationSuccess, ToolError> {
    let budget = context.timeout_budget_ms.unwrap_or(30_000);
    if use_context
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.is_cancelled())
    {
        return Err(ToolError::Aborted);
    }
    if budget == 0 {
        return Err(ToolError::Internal(format!(
            "{}: audio operation exceeded its deadline",
            AudioErrorKind::Timeout,
        )));
    }
    let deadline = Duration::from_millis(budget);
    let cancel = use_context.cancel.clone();
    let operation_future = service.execute(context, operation);

    if let Some(cancel) = cancel {
        tokio::select! {
            biased;
            result = operation_future => result.map_err(map_audio_error),
            _ = tokio::time::sleep(deadline) => Err(ToolError::Internal(format!(
                "{}: audio operation exceeded its deadline",
                AudioErrorKind::Timeout,
            ))),
            _ = cancel.cancelled() => Err(ToolError::Aborted),
        }
    } else {
        tokio::select! {
            biased;
            result = operation_future => result.map_err(map_audio_error),
            _ = tokio::time::sleep(deadline) => Err(ToolError::Internal(format!(
                "{}: audio operation exceeded its deadline",
                AudioErrorKind::Timeout,
            ))),
        }
    }
}

pub(crate) fn map_audio_error(error: AudioError) -> ToolError {
    match error.kind {
        AudioErrorKind::PermissionDenied => ToolError::PermissionDenied(error.message),
        AudioErrorKind::Cancelled => ToolError::Aborted,
        AudioErrorKind::InvalidRequest => ToolError::InvalidInput(error.message),
        kind => ToolError::Internal(format!("{kind}: {}", error.message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::audio::{
        AudioCapabilitySnapshot, AudioOperationReadiness, AudioReadinessState,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropService {
        dropped: Arc<AtomicBool>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        capabilities: AudioCapabilitySnapshot,
    }

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl AudioService for DropService {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            self.capabilities.clone()
        }

        async fn execute(
            &self,
            _context: AudioOperationContext,
            _operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _drop_flag = DropFlag(self.dropped.clone());
            std::future::pending().await
        }

        async fn cancel(&self, _identity: AudioOperationId) -> Result<(), AudioError> {
            Ok(())
        }
    }

    fn capabilities() -> AudioCapabilitySnapshot {
        AudioCapabilitySnapshot {
            service_epoch: 7,
            support_revision: 1,
            supported_operations: vec![AudioOperationKind::Speak],
            readiness: vec![AudioOperationReadiness {
                operation: AudioOperationKind::Speak,
                state: AudioReadinessState::Ready,
            }],
            max_payload_bytes: 4096,
        }
    }

    fn use_context() -> ToolUseContext {
        let mut context = tool_api::test_support::fresh_ctx();
        context.origin_session_id = Some(lingxi_core::types::SessionId::new());
        context
    }

    #[tokio::test]
    async fn cancellation_and_deadline_drop_the_exact_execute_future() {
        let dropped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let service: Arc<dyn AudioService> = Arc::new(DropService {
            dropped: dropped.clone(),
            calls: calls.clone(),
            capabilities: capabilities(),
        });
        let mut call_context = use_context();
        let cancel = tokio_util::sync::CancellationToken::new();
        call_context.cancel = Some(cancel.clone());
        let operation_request = operation_context(&service, &call_context, Duration::from_secs(5))
            .await
            .unwrap();
        let task = tokio::spawn({
            let service = service.clone();
            async move {
                execute_operation(
                    &service,
                    &call_context,
                    operation_request,
                    AudioOperation::Speak {
                        text: "hello".into(),
                        language: None,
                        rate: None,
                        voice: None,
                    },
                )
                .await
            }
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), Err(ToolError::Aborted)));
        assert!(dropped.load(Ordering::SeqCst));

        dropped.store(false, Ordering::SeqCst);
        let deadline_context =
            operation_context(&service, &use_context(), Duration::from_millis(5))
                .await
                .unwrap();
        let result = execute_operation(
            &service,
            &use_context(),
            deadline_context,
            AudioOperation::Speak {
                text: "hello".into(),
                language: None,
                rate: None,
                voice: None,
            },
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::Internal(message)) if message.starts_with("timeout:"))
        );
        assert!(dropped.load(Ordering::SeqCst));

        dropped.store(false, Ordering::SeqCst);
        calls.store(0, Ordering::SeqCst);
        let mut cancelled_context = use_context();
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        cancelled_context.cancel = Some(cancellation);
        let operation = operation_context(&service, &cancelled_context, Duration::from_secs(5))
            .await
            .unwrap();
        let result = execute_operation(
            &service,
            &cancelled_context,
            operation,
            AudioOperation::Speak {
                text: "hello".into(),
                language: None,
                rate: None,
                voice: None,
            },
        )
        .await;
        assert!(matches!(result, Err(ToolError::Aborted)));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "pre-cancelled calls must not enter the native service"
        );
        assert!(!dropped.load(Ordering::SeqCst));
    }
}
