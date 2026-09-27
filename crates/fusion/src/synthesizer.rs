//! Merge synthesizer: exactly one side query to the configured synthesizer
//! route (`fusion.synthesizerModel`), which defaults to nothing — like every
//! other Fusion role it must be named in settings before a run may start.

use crate::config::FusionRuntimeConfig;
use crate::model_resolver::{ModelLimits, ResolvedPanel};
use crate::packing::{self, PackingError};
use crate::panel::PanelInternal;
use platform_api::subagent_output_guard::sanitize_blocks;
use platform_api::{FusionAnalysis, FusionRequest};
use sidequery::{SideQueryClient, SideQueryError, SideQueryResponse};
// Used only by the tests below. Kept at file scope behind `cfg(test)` so the
// lib target carries no unused import (which `clippy --fix` deletes) while the
// test module still reaches it through `use super::*`.
#[cfg(test)]
use sidequery::SideQueryRequest;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Synthesizer-stage failure.
///
/// [Round-5 review item 9] Always returned PAIRED with the `cost::Usage`
/// the failed call had already been billed for (`cost::Usage::default()`
/// when the provider reported none) — the same shape `analyst::AnalystUsage`
/// gives the analyst stage. Before this, `synthesize`'s empty-text arm
/// destructured a `SideQueryResponse` that carried real, provider-reported
/// usage and then returned a payload-less `Failed`, so the caller re-priced
/// a completed, billed call as an input-only ESTIMATE with zero output and
/// zero reasoning tokens — the one place in the run where the true figure
/// was already in hand at the moment it was thrown away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynthError {
    /// Successful billed output cited a reference absent from the final payload.
    InvalidCitations,
    /// Provider / protocol failure.
    Failed,
    /// Idle / total timeout.
    TimedOut,
}

/// Run the merge using the route limits captured at preparation.
#[cfg(test)]
pub(crate) async fn synthesize_with_limits(
    client: Arc<dyn SideQueryClient>,
    config: &FusionRuntimeConfig,
    request: &FusionRequest,
    synth_route: &ResolvedPanel,
    analysis: &FusionAnalysis,
    panels: &[PanelInternal],
    limits: ModelLimits,
) -> Result<(String, cost::Usage), (SynthError, cost::Usage)> {
    synthesize_registered(
        client,
        config,
        request,
        synth_route,
        analysis,
        panels,
        limits,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn synthesize_registered(
    client: Arc<dyn SideQueryClient>,
    config: &FusionRuntimeConfig,
    request: &FusionRequest,
    synth_route: &ResolvedPanel,
    analysis: &FusionAnalysis,
    panels: &[PanelInternal],
    limits: ModelLimits,
    attempt_run: Option<&platform_api::ModelAttemptRun>,
) -> Result<(String, cost::Usage), (SynthError, cost::Usage)> {
    let output_tokens = limits.output_cap(config.synthesizer_max_output_tokens);
    let prepared = match packing::prepare_synth_request(
        client.as_ref(),
        request,
        synth_route,
        analysis,
        panels,
        output_tokens,
        limits,
    ) {
        Ok(req) => req,
        Err(_) => return Err((SynthError::Failed, cost::Usage::default())),
    };
    let allowed_citations = prepared.allowed_citations;
    let mut req = prepared.request;
    if let Some(run) = attempt_run {
        req.model_attempt = match run.context(platform_api::ModelAttemptStage::Synthesis, None) {
            Ok(context) => Some(context),
            Err(_) => return Err((SynthError::Failed, cost::Usage::default())),
        };
    }
    let outcome = timeout(
        Duration::from_millis(config.synthesizer_timeout_ms),
        client.query(req),
    )
    .await;
    match outcome {
        // The call was cancelled mid-flight by our own timeout: no usage
        // was ever reported to this side.
        Err(_) => Err((SynthError::TimedOut, cost::Usage::default())),
        // [Round-5 review item 9] `Partial` is the second arm that arrives
        // with real usage in hand — "one or more batches completed before a
        // later batch failed. The partial accounting must still be charged"
        // (its own doc). Hand it back rather than dropping it.
        Ok(Err(SideQueryError::Partial { usage, .. })) => Err((SynthError::Failed, usage)),
        Ok(Err(
            SideQueryError::Api(_)
            | SideQueryError::InvalidResponse(_)
            | SideQueryError::StructuredOutputUnsupported,
        )) => Err((SynthError::Failed, cost::Usage::default())),
        Ok(Ok(SideQueryResponse { text, usage, .. })) => {
            let raw = text.unwrap_or_default();
            if raw.trim().is_empty() {
                // [Round-5 review item 9] A completed, billed call whose
                // content held no text block (a reasoning-only completion
                // that hit `max_tokens`, or a refusal) — `usage` here is the
                // provider's own figure for tokens that were already paid
                // for, so it must survive the failure.
                return Err((SynthError::Failed, usage));
            }
            let sanitized = sanitize_blocks(&[raw.replace('\0', "")]).content.join("");
            if crate::citations::validate_merged_citations(&sanitized, &allowed_citations).is_err()
            {
                return Err((SynthError::InvalidCitations, usage));
            }
            Ok((sanitized, usage))
        }
    }
}

/// Prepare the synthesizer payload before the stage is marked as attempted.
pub(crate) fn preflight_request(
    client: &dyn SideQueryClient,
    config: &FusionRuntimeConfig,
    request: &FusionRequest,
    synth_route: &ResolvedPanel,
    analysis: &FusionAnalysis,
    panels: &[PanelInternal],
    limits: ModelLimits,
) -> Result<(), PackingError> {
    packing::preflight_synth_request(
        client,
        request,
        synth_route,
        analysis,
        panels,
        limits.output_cap(config.synthesizer_max_output_tokens),
        limits,
    )
}

pub(crate) fn estimate_input_tokens(
    client: &dyn SideQueryClient,
    config: &FusionRuntimeConfig,
    request: &FusionRequest,
    synth_route: &ResolvedPanel,
    analysis: &FusionAnalysis,
    panels: &[PanelInternal],
    limits: ModelLimits,
) -> Result<u64, PackingError> {
    packing::estimate_synth_request(
        client,
        request,
        synth_route,
        analysis,
        panels,
        limits.output_cap(config.synthesizer_max_output_tokens),
        limits,
    )
    .map(|estimate| estimate.input_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use platform_api::{FusionOrigin, FusionPreset, FusionRecommendation};
    use std::sync::Mutex;

    async fn synthesize_with_test_limits(
        client: Arc<dyn SideQueryClient>,
        config: &FusionRuntimeConfig,
        request: &FusionRequest,
        analysis: &FusionAnalysis,
        panels: &[PanelInternal],
    ) -> Result<(String, cost::Usage), (SynthError, cost::Usage)> {
        let synth_route = ResolvedPanel {
            profile: request.parent_profile.clone(),
            model: request.parent_model.clone(),
        };
        synthesize_with_limits(
            client,
            config,
            request,
            &synth_route,
            analysis,
            panels,
            crate::model_resolver::known_test_limits(),
        )
        .await
    }

    struct CapturingClient {
        captured: Mutex<Option<SideQueryRequest>>,
    }

    #[async_trait]
    impl SideQueryClient for CapturingClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            *self.captured.lock().unwrap() = Some(request);
            Ok(SideQueryResponse {
                text: Some("merged answer".into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    fn stub_request() -> FusionRequest {
        FusionRequest {
            schema_version: 1,
            origin: FusionOrigin::Slash,
            prompt: "task".into(),
            preset: FusionPreset::Quality,
            models: None,
            dimensions: vec!["coverage".into()],
            partial_ok: true,
            max_panel: None,
            cross_provider: true,
            parent_profile: "anthropic".into(),
            parent_model: "claude-sonnet-5".into(),
            workflow_run_id: None,
        }
    }

    fn stub_analysis() -> FusionAnalysis {
        FusionAnalysis {
            schema_version: 1,
            consensus: vec![],
            contradictions: vec![],
            unique_insights: vec![],
            coverage_gaps: vec![],
            scores: std::collections::BTreeMap::new(),
            confidence: 80,
            recommendation: FusionRecommendation::Merge {
                reason: "complementary".into(),
            },
        }
    }

    /// A client whose completion carried real, billed usage but no text
    /// block at all — the reasoning-only / refusal / `max_tokens` shape
    /// `provider_side_query::decode_response` renders as `text: None`
    /// (it accumulates only `Text` blocks and ignores `Reasoning` ones).
    struct BilledButTextlessClient;

    struct BilledCitationClient {
        answer: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl SideQueryClient for BilledCitationClient {
        async fn query(&self, _: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SideQueryResponse {
                text: Some(self.answer.clone()),
                structured: None,
                tool_calls: vec![],
                usage: cost::Usage {
                    tokens: cost::TokenUsage {
                        input: 4_000,
                        output: 3,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    #[tokio::test]
    async fn citation_integrity_preserves_billed_usage_without_an_extra_query() {
        let panel = PanelInternal {
            index: 0,
            profile: "p".into(),
            model: "m".into(),
            anonymous_id: "P1".into(),
            status: platform_api::PanelRunStatus::Completed,
            report: Some(platform_api::PanelReport {
                schema_version: 1,
                summary: "summary".into(),
                candidate_answer: "candidate".into(),
                claims: vec![],
                evidence: vec![platform_api::PanelEvidence {
                    id: "e1".into(),
                    kind: platform_api::EvidenceKind::File,
                    locator: "a.rs".into(),
                    excerpt: None,
                }],
                assumptions: vec![],
                risks: vec![],
                unresolved_questions: vec![],
            }),
            duration_ms: 0,
            error_category: None,
            error_detail: None,
            usage: None,
            spawn_prompt: String::new(),
        };
        for (answer, valid) in [
            ("answer [evidence:P1:e1]".to_string(), true),
            ("answer with no citation".into(), true),
            // An id the panel never listed, and one listed under another panel.
            ("answer [evidence:P1:e9]".into(), false),
            ("answer [evidence:P2:e1]".into(), false),
        ] {
            let client = Arc::new(BilledCitationClient {
                answer,
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let outcome = synthesize_with_test_limits(
                client.clone(),
                &FusionRuntimeConfig::defaults(),
                &stub_request(),
                &stub_analysis(),
                std::slice::from_ref(&panel),
            )
            .await;
            let usage = if valid {
                outcome.unwrap().1
            } else {
                let (error, usage) = outcome.unwrap_err();
                assert_eq!(error, SynthError::InvalidCitations);
                usage
            };
            assert_eq!(usage.tokens.input, 4_000);
            assert_eq!(usage.tokens.output, 3);
            assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[async_trait]
    impl SideQueryClient for BilledButTextlessClient {
        async fn query(
            &self,
            _request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            Ok(SideQueryResponse {
                text: None,
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage {
                    tokens: cost::TokenUsage {
                        input: 4_000,
                        output: 0,
                        reasoning_output: 16_384,
                        ..cost::TokenUsage::default()
                    },
                    ..cost::Usage::default()
                },
                stop_reason: Some("max_tokens".into()),
                retry_count: 0,
            })
        }
    }

    /// [Round-5 review item 9] The failure must hand the caller the usage
    /// the provider already billed. Before this, the empty-text arm
    /// destructured `usage` out of the response and then returned a
    /// payload-less `SynthError::Failed`, so 16,384 already-billed
    /// reasoning tokens were re-priced by the caller as an input-only
    /// estimate with output and reasoning hard-coded to 0.
    #[tokio::test]
    async fn usage_survives_a_billed_response_with_no_text_block() {
        let client: Arc<dyn SideQueryClient> = Arc::new(BilledButTextlessClient);
        let config = FusionRuntimeConfig::defaults();
        let (error, usage) =
            synthesize_with_test_limits(client, &config, &stub_request(), &stub_analysis(), &[])
                .await
                .expect_err("an empty-text response is still a synthesizer failure");
        assert_eq!(error, SynthError::Failed);
        assert_eq!(
            usage.tokens.reasoning_output, 16_384,
            "the reasoning tokens the provider billed must survive the failure"
        );
        assert_eq!(
            usage.tokens.input, 4_000,
            "the input tokens the provider billed must survive the failure"
        );
    }

    #[tokio::test]
    async fn system_prompt_frames_panels_and_analysis_as_untrusted_data() {
        let concrete = Arc::new(CapturingClient {
            captured: Mutex::new(None),
        });
        let client: Arc<dyn SideQueryClient> = concrete.clone();
        let config = FusionRuntimeConfig::defaults();
        let request = stub_request();
        let analysis = stub_analysis();

        synthesize_with_test_limits(client, &config, &request, &analysis, &[])
            .await
            .expect("synth call succeeds");

        let captured = concrete
            .captured
            .lock()
            .unwrap()
            .clone()
            .expect("request reached the client");
        let system_prompt = captured
            .system_prompt
            .expect("system prompt set")
            .to_lowercase();
        assert!(
            system_prompt.contains("untrusted"),
            "synthesizer system prompt must frame panels/analysis as untrusted data: {system_prompt}"
        );
        assert!(system_prompt.contains("never follow"));
    }
}
