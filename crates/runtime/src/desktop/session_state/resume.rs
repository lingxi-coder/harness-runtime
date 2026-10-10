//! One-time cold-resume/fork opening state, owned by the existing ledger.

use super::*;
use cost::{ModelRef, ModelUsage, ProviderId, ServerToolUsage, TokenUsage, Usage};
use lingxi_core::host::live_sessions::SharedSessionWriterLease;

/// Capture while the caller holds the source writer claim. An existing WAL
/// always wins over the native compatibility record; a broken WAL never falls
/// back to a cheaper or empty transcript cost.
pub(crate) async fn capture_resume_cost(
    home: &Path,
    source_id: uuid::Uuid,
    lease: SharedSessionWriterLease,
    transcript: &str,
) -> Result<Option<CostState>, String> {
    let source_id = SessionId::from_uuid(source_id);
    let directory = home
        .join("session-state")
        .join(source_id.as_uuid().to_string());
    if tokio::fs::try_exists(directory.join(session::jsonl::JOURNAL_FILE_NAME))
        .await
        .map_err(|error| error.to_string())?
        || tokio::fs::try_exists(directory.join(session::jsonl::SNAPSHOT_FILE_NAME))
            .await
            .map_err(|error| error.to_string())?
    {
        let home = home.to_path_buf();
        return tokio::task::spawn_blocking(move || {
            let coordinator = SessionStateCoordinator::open(&home, source_id, lease)
                .map_err(|error| error.to_string())?;
            coordinator
                .hydrate_blocking()
                .map(|hydration| Some(hydration.state))
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?;
    }
    native_opening_state(transcript, source_id)
}

impl SessionStateCoordinator {
    /// Import a frozen source aggregate only into an empty target WAL. The
    /// target event is administrative opening state, never another model
    /// receipt, and no source ledger or global provider accounting is changed.
    pub async fn import_session_opening_state(
        &self,
        opening: &CostState,
    ) -> Result<Option<CostPersistAck>, CostPersistError> {
        let mutation_id = CostMutationId::new(format!(
            "session-opening-state:v1:{}:{}",
            opening.session_id, self.state.session_id,
        ));
        let state = self.state.clone();
        let lookup_id = mutation_id.clone();
        if let Some(ack) = tokio::task::spawn_blocking(move || {
            let entry = state
                .journal
                .find_event_durable(lookup_id.as_str())
                .map_err(map_journal_error)?;
            let Some(entry) = entry else { return Ok(None) };
            let SessionEvent::Cost(record) = decode_session_event(entry.event)? else {
                return Err(CostPersistError::Storage(
                    "opening state id belongs to another event".into(),
                ));
            };
            if record.source != CostMutationSource::SessionOpeningState
                || record.state.session_id != state.session_id
            {
                return Err(CostPersistError::Storage(
                    "opening state receipt identity mismatch".into(),
                ));
            }
            Ok::<_, CostPersistError>(Some(CostPersistAck {
                mutation_id: lookup_id,
                journal_revision: entry.journal_revision,
                cost_revision: record.cost_revision,
            }))
        })
        .await
        .map_err(|error| CostPersistError::Storage(error.to_string()))??
        {
            return Ok(Some(ack));
        }
        let permit = <Self as CostPersistence>::acquire_permit(self, self.state.session_id).await?;
        let acknowledgement = {
            let projection = self
                .state
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some((current, journal_revision)) = projection.latest.as_ref() else {
                return Err(CostPersistError::Rejected(
                    "opening state needs a hydrated coordinator".into(),
                ));
            };
            if *journal_revision != 0 || current.cost_revision != 0 {
                return Ok(None);
            }
            let mut inherited = opening.clone();
            inherited.session_id = self.state.session_id;
            inherited.cost_revision = 1;
            inherited.legacy_import_evaluated = true;
            // Attempt reservations belong to the source owner. Only realized
            // cumulative usage is an inherited display/budget baseline.
            inherited.unverified_nano_usd = 0;
            inherited.last_usage_revision = inherited.last_usage.as_ref().map(|_| 1);
            let (ack, received) = tokio::sync::oneshot::channel();
            permit.enqueue(CostPersistRequest {
                session_id: self.state.session_id,
                cost_revision: 1,
                mutation_id,
                state: CostStateVector::from(&inherited),
                source: CostMutationSource::SessionOpeningState,
                ack,
            })?;
            received
        };
        acknowledgement
            .await
            .map_err(|_| CostPersistError::Storage("opening state acknowledgement dropped".into()))?
            .map(Some)
    }
}

fn native_opening_state(
    transcript: &str,
    session_id: SessionId,
) -> Result<Option<CostState>, String> {
    let sid = session_id.as_uuid().to_string();
    let row = transcript
        .lines()
        .filter_map(|line| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line)
                .ok()
                .map(|projection| projection.value)
        })
        .filter(|row| row["type"] == "cost-state" && row["sessionId"] == sid)
        .last();
    let Some(row) = row else { return Ok(None) };
    let mut state = CostState {
        session_id,
        total_nano_usd: nano_usd(&row["totalCostUSD"])?,
        total_api_duration_ms: counter(&row, "totalAPIDuration"),
        total_api_duration_without_retries_ms: counter(&row, "totalAPIDurationWithoutRetries"),
        total_tool_duration_ms: counter(&row, "totalToolDuration"),
        total_lines_added: counter(&row, "totalLinesAdded"),
        total_lines_removed: counter(&row, "totalLinesRemoved"),
        ..Default::default()
    };
    if let Some(models) = row["modelUsage"].as_object() {
        for (model, usage) in models {
            let provider = match usage["provider"].as_str() {
                None | Some("firstParty") => ProviderId::Anthropic,
                Some("bedrock") => ProviderId::AmazonBedrock,
                Some("openai") => ProviderId::OpenAI,
                Some("gemini") => ProviderId::GoogleGemini,
                Some(name) => ProviderId::Custom {
                    name: name.to_owned(),
                },
            };
            let model_ref = ModelRef {
                provider,
                model: model.clone(),
            };
            let web_search_requests = u32::try_from(counter(usage, "webSearchRequests"))
                .map_err(|_| "native web-search counter overflow")?;
            state.total_web_search_requests = state
                .total_web_search_requests
                .saturating_add(web_search_requests);
            let model_usage = ModelUsage {
                model_ref: model_ref.clone(),
                usage: Usage {
                    tokens: TokenUsage {
                        input: counter(usage, "inputTokens"),
                        output: counter(usage, "outputTokens"),
                        reasoning_output: counter(usage, "thinkingTokens"),
                        cache_read: counter(usage, "cacheReadInputTokens"),
                        cache_write: counter(usage, "cacheCreationInputTokens"),
                        ..Default::default()
                    },
                    server_tool_use: Some(ServerToolUsage {
                        web_search_requests,
                    }),
                    ..Default::default()
                },
                cache_read_input_tokens: counter(usage, "cacheReadInputTokens"),
                cache_creation_input_tokens: counter(usage, "cacheCreationInputTokens"),
                cost_nano_usd: nano_usd(&usage["costUSD"])?,
            };
            if row["hasUnknownModelCost"].as_bool() == Some(true) && model_usage.cost_nano_usd == 0
            {
                state.unpriced_models.insert(model_ref.clone());
            }
            state.per_model_usage.insert(model_ref, model_usage);
        }
    }
    Ok(Some(state))
}

fn counter(row: &serde_json::Value, name: &str) -> u64 {
    row[name].as_u64().unwrap_or(0)
}

fn nano_usd(value: &serde_json::Value) -> Result<u64, String> {
    let usd = value
        .as_f64()
        .ok_or("native cost state has no numeric USD value")?;
    if !usd.is_finite() || usd < 0.0 || usd > u64::MAX as f64 / 1_000_000_000.0 {
        return Err("native cost state USD value is out of range".into());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok((usd * 1_000_000_000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Lease(String);
    impl lingxi_core::host::live_sessions::SessionWriterLease for Lease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }
    fn open(home: &Path, id: SessionId) -> Arc<SessionStateCoordinator> {
        SessionStateCoordinator::open(home, id, Arc::new(Lease(id.to_string()))).unwrap()
    }

    #[tokio::test]
    async fn opening_usage_is_once_only_across_retry_and_reopen_without_source_changes() {
        let home = tempfile::tempdir().unwrap();
        let source_id = SessionId::new();
        let source = open(home.path(), source_id);
        let source_worker = source.start().await.unwrap();
        let mut opening = CostState {
            session_id: source_id,
            cost_revision: 1,
            total_nano_usd: 22_000,
            unverified_nano_usd: 91,
            ..Default::default()
        };
        let model_ref = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-sonnet-5-5".into(),
        };
        opening.per_model_usage.insert(
            model_ref.clone(),
            ModelUsage {
                model_ref,
                usage: Usage {
                    tokens: TokenUsage {
                        input: 1,
                        output: 2,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_nano_usd: 22_000,
            },
        );
        let permit = source.acquire_permit(source_id).await.unwrap();
        let (ack, result) = tokio::sync::oneshot::channel();
        permit
            .enqueue(CostPersistRequest {
                session_id: source_id,
                cost_revision: 1,
                mutation_id: CostMutationId::new("source-response"),
                state: CostStateVector::from(&opening),
                source: CostMutationSource::ModelResponse,
                ack,
            })
            .unwrap();
        result.await.unwrap().unwrap();
        source.close_and_drain().await.unwrap();
        drop(source);
        source_worker.await.unwrap();
        let source_path = home
            .path()
            .join("session-state")
            .join(source_id.as_uuid().to_string())
            .join(session::jsonl::JOURNAL_FILE_NAME);
        let source_before = tokio::fs::read(&source_path).await.unwrap();

        let target_id = SessionId::new();
        let target = open(home.path(), target_id);
        let worker = target.start().await.unwrap();
        let first = target
            .import_session_opening_state(&opening)
            .await
            .unwrap()
            .unwrap();
        let second = target
            .import_session_opening_state(&opening)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, second);
        target
            .import_legacy_opening_balance(Some(900_000))
            .await
            .unwrap();
        let hydrated = target.hydrate(target_id).await.unwrap().state;
        assert_eq!(hydrated.total_nano_usd, 22_000);
        assert_eq!(hydrated.per_model_usage, opening.per_model_usage);
        assert_eq!(hydrated.unverified_nano_usd, 0);
        let events = target.journal().replay().unwrap().entries;
        assert_eq!(events.len(), 1);
        assert!(
            matches!(decode_session_event(events[0].event.clone()).unwrap(), SessionEvent::Cost(record) if record.source == CostMutationSource::SessionOpeningState)
        );
        target.close_and_drain().await.unwrap();
        drop(target);
        worker.await.unwrap();

        let reopened = open(home.path(), target_id);
        let worker = reopened.start().await.unwrap();
        opening.total_nano_usd = 99_000;
        assert_eq!(
            reopened
                .import_session_opening_state(&opening)
                .await
                .unwrap(),
            Some(first)
        );
        assert_eq!(
            reopened
                .hydrate(target_id)
                .await
                .unwrap()
                .state
                .total_nano_usd,
            22_000
        );
        assert_eq!(tokio::fs::read(source_path).await.unwrap(), source_before);
        reopened.close_and_drain().await.unwrap();
        drop(reopened);
        worker.await.unwrap();
    }

    #[test]
    fn native_cost_state_retains_cumulative_model_usage() {
        let id = SessionId::new();
        let raw = serde_json::json!({
            "type":"cost-state", "sessionId":id.as_uuid().to_string(), "totalCostUSD":0.000022,
            "totalAPIDuration":9, "totalAPIDurationWithoutRetries":7,
            "modelUsage":{"claude-sonnet-5-5":{"inputTokens":1,"outputTokens":2,"thinkingTokens":0,
                "cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.000022}},
            "hasUnknownModelCost":false,
        });
        let state = native_opening_state(&raw.to_string(), id).unwrap().unwrap();
        assert_eq!(state.total_nano_usd, 22_000);
        assert_eq!(state.total_api_duration_ms, 9);
        assert_eq!(
            state
                .per_model_usage
                .values()
                .next()
                .unwrap()
                .usage
                .tokens
                .output,
            2
        );
    }
}
