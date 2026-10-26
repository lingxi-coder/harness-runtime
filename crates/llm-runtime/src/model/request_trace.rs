//! Request spans cover both dispatch and consumption of a streamed response.
//! No request/response content or provider error text enters diagnostic data.

use crate::{HistoryEvent, HistoryResponse, LlmError};
use futures::{stream::BoxStream, StreamExt};
use telemetry::otel::{start_operation_span, with_trace_context_future, AttrValue, Attributes};

fn attributes(stream: bool) -> Attributes {
    let mut attributes = Attributes::new();
    attributes.insert("stream".into(), AttrValue::Bool(stream));
    attributes
}

pub(crate) async fn non_stream(
    future: impl std::future::Future<Output = Result<HistoryResponse, LlmError>>,
) -> Result<HistoryResponse, LlmError> {
    let span = start_operation_span("lingxi.llm.request", attributes(false));
    let context = span.trace_context();
    let result = with_trace_context_future(context.as_ref(), future).await;
    span.set_outcome(if result.is_ok() { "success" } else { "error" });
    result
}

pub(crate) async fn stream(
    future: impl std::future::Future<
        Output = Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>,
    >,
) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
    let span = start_operation_span("lingxi.llm.request", attributes(true));
    let context = span.trace_context();
    let stream = match with_trace_context_future(context.as_ref(), future).await {
        Ok(stream) => stream,
        Err(error) => {
            span.set_outcome("error");
            return Err(error);
        }
    };
    // Keep the owning span in the returned stream. Ending it when headers arrive
    // would hide token latency and detach all telemetry produced while polling.
    Ok(futures::stream::unfold(
        (stream, span, context, false),
        |(mut stream, span, context, mut terminal)| async move {
            let item = with_trace_context_future(context.as_ref(), stream.next()).await;
            match &item {
                Some(Ok(HistoryEvent::MessageStop | HistoryEvent::Completed { .. })) => {
                    terminal = true;
                    span.set_outcome("success");
                }
                Some(Err(_)) => {
                    terminal = true;
                    span.set_outcome("error");
                }
                None if !terminal => span.set_outcome("error"),
                _ => {}
            }
            item.map(|item| (item, (stream, span, context, terminal)))
        },
    )
    .boxed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use telemetry::otel::{capture_current_trace_context, OtelConfig, SerializedTraceContext};

    #[tokio::test]
    async fn request_context_survives_returning_the_stream_and_each_poll() {
        let config = OtelConfig::from_lookup(|key| match key {
            "LINGXI_ENABLE_TELEMETRY" => Some("true".into()),
            "OTEL_TRACES_EXPORTER" => Some("console".into()),
            "OTEL_METRICS_EXPORTER" | "OTEL_LOGS_EXPORTER" => Some("none".into()),
            _ => None,
        });
        let _guard =
            telemetry::otel::install_process_with_config("request-trace-test", false, config);
        let parent = SerializedTraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: None,
        };
        let observations = Arc::new(Mutex::new(Vec::new()));
        let poll_observations = observations.clone();
        let init_observations = observations.clone();
        let mut response = with_trace_context_future(
            Some(&parent),
            stream(async move {
                init_observations
                    .lock()
                    .unwrap()
                    .push(capture_current_trace_context().unwrap());
                Ok(futures::stream::unfold(0, move |index| {
                    let observations = poll_observations.clone();
                    async move {
                        observations
                            .lock()
                            .unwrap()
                            .push(capture_current_trace_context().unwrap());
                        (index == 0).then_some((Ok(HistoryEvent::MessageStop), index + 1))
                    }
                })
                .boxed())
            }),
        )
        .await
        .unwrap();
        assert!(capture_current_trace_context().is_none());
        assert!(matches!(
            response.next().await,
            Some(Ok(HistoryEvent::MessageStop))
        ));
        assert!(response.next().await.is_none());
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 3);
        assert_eq!(observations[0], observations[1]);
        assert_eq!(observations[1], observations[2]);
        assert_eq!(
            &observations[0].traceparent[3..35],
            &parent.traceparent[3..35]
        );
        assert_ne!(
            &observations[0].traceparent[36..52],
            &parent.traceparent[36..52]
        );
        assert!(capture_current_trace_context().is_none());
    }
}
