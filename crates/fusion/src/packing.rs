//! Deterministic request construction and route-capacity packing for judge calls.
//!
//! The full request is always tried first.  Only when the canonical request
//! estimate does not fit do we replace optional report material with stable
//! anonymous-id placeholders.  The task, dimensions, every panel id, and
//! every critical risk remain in the packed form; omission counters make the
//! loss explicit to the judge and to tests.

use crate::model_resolver::{ModelLimits, ResolvedPanel};
use crate::panel::PanelInternal;
use platform_api::subagent_output_guard::sanitize_blocks;
use platform_api::{FusionRequest, PanelReport, RiskSeverity};
use protocol::{ConversationMessage, MessageId};
use serde_json::{json, Value};
use sidequery::{
    CanonicalSideQueryRequest, QuerySource, SideQueryClient, SideQueryError,
    StrictStructuredQueryRequest,
};
const RETRY_HINT_BYTE_CAP: usize = 512;

/// Why a judge request could not be prepared without contacting a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PackingError {
    /// The selected route did not publish enough capacity metadata.
    UnknownCapacity,
    /// Metadata exists but leaves no usable input or output tokens.
    UnusableCapacity,
    /// The mandatory task/metadata/id/risk portion cannot fit.
    MandatoryTooLarge { input_tokens: u64, input_cap: u64 },
    /// The shared estimator rejected the canonical request.
    Estimate(String),
}

impl PackingError {
    pub(crate) fn category(&self) -> &'static str {
        match self {
            Self::UnknownCapacity => "unknown_capacity",
            Self::UnusableCapacity => "unusable_capacity",
            Self::MandatoryTooLarge { .. } => "mandatory_input_too_large",
            Self::Estimate(_) => "request_estimate_failed",
        }
    }
}

/// Build an analyst request and prove it fits the selected route.  The first
/// request is byte-for-byte the legacy full payload; packing is a fallback,
/// never a semantic change on routes where the payload fits.
pub(crate) fn prepare_analyst_request(
    client: &dyn SideQueryClient,
    request: &FusionRequest,
    analyst: &ResolvedPanel,
    panels: &[PanelInternal],
    schema: Value,
    system_prompt: String,
    retry_hint: Option<&str>,
    output_tokens: u32,
    limits: ModelLimits,
) -> Result<StrictStructuredQueryRequest, PackingError> {
    let input_cap = usable_input_cap(limits, output_tokens)?;
    let full = strict_request(
        analyst,
        analyst_user_message(request, panels, retry_hint),
        schema.clone(),
        output_tokens,
        &system_prompt,
    );
    if fits(
        client,
        CanonicalSideQueryRequest::Strict(full.clone()),
        limits,
        output_tokens,
    )? {
        return Ok(full);
    }

    let sources = analyst_packed_sources(panels);
    let mandatory = strict_request(
        analyst,
        packed_analyst_user_message(request, retry_hint, &sources, 0, OmissionMode::Actual),
        schema.clone(),
        output_tokens,
        &system_prompt,
    );
    let mandatory_estimate = estimate(client, CanonicalSideQueryRequest::Strict(mandatory))?;
    if mandatory_estimate.input_tokens > input_cap {
        return Err(PackingError::MandatoryTooLarge {
            input_tokens: mandatory_estimate.input_tokens,
            input_cap,
        });
    }

    // Max-min water filling apportions bytes across anonymous panels first,
    // then across that panel's optional groups. Short units return their
    // unused share; no early P-id or many-field report can consume the rest.
    let mut low = 0_usize;
    let mut high = total_optional_bytes(&sources);
    while low < high {
        let candidate_budget = low.saturating_add(high.saturating_sub(low).div_ceil(2));
        let candidate_request = strict_request(
            analyst,
            packed_analyst_user_message(
                request,
                retry_hint,
                &sources,
                candidate_budget,
                OmissionMode::Conservative,
            ),
            schema.clone(),
            output_tokens,
            &system_prompt,
        );
        if fits(
            client,
            CanonicalSideQueryRequest::Strict(candidate_request),
            limits,
            output_tokens,
        )? {
            low = candidate_budget;
        } else {
            high = candidate_budget.saturating_sub(1);
        }
    }

    let packed = strict_request(
        analyst,
        packed_analyst_user_message(request, retry_hint, &sources, low, OmissionMode::Actual),
        schema,
        output_tokens,
        &system_prompt,
    );
    let estimate = estimate(client, CanonicalSideQueryRequest::Strict(packed.clone()))?;
    if estimate.input_tokens > input_cap {
        return Err(PackingError::MandatoryTooLarge {
            input_tokens: estimate.input_tokens,
            input_cap,
        });
    }
    Ok(packed)
}

/// Cheap preflight used by the orchestrator before it marks a stage as an
/// attempted provider call.  The actual stage calls this same builder again;
/// keeping the operation pure means a mandatory no-fit path cannot egress.
pub(crate) fn preflight_analyst_request(
    client: &dyn SideQueryClient,
    request: &FusionRequest,
    analyst: &ResolvedPanel,
    panels: &[PanelInternal],
    schema: Value,
    system_prompt: String,
    reserve_retry_hint: bool,
    output_tokens: u32,
    limits: ModelLimits,
) -> Result<(), PackingError> {
    let retry_hint = reserve_retry_hint.then(|| "\u{0001}".repeat(RETRY_HINT_BYTE_CAP));
    prepare_analyst_request(
        client,
        request,
        analyst,
        panels,
        schema,
        system_prompt,
        retry_hint.as_deref(),
        output_tokens,
        limits,
    )
    .map(|_| ())
}

pub(crate) fn estimate_analyst_request(
    client: &dyn SideQueryClient,
    request: &FusionRequest,
    analyst: &ResolvedPanel,
    panels: &[PanelInternal],
    schema: Value,
    system_prompt: String,
    reserve_retry_hint: bool,
    output_tokens: u32,
    limits: ModelLimits,
) -> Result<sidequery::SideQueryEstimate, PackingError> {
    let retry_hint = reserve_retry_hint.then(|| "\u{0001}".repeat(RETRY_HINT_BYTE_CAP));
    let prepared = prepare_analyst_request(
        client,
        request,
        analyst,
        panels,
        schema,
        system_prompt,
        retry_hint.as_deref(),
        output_tokens,
        limits,
    )?;
    estimate(client, CanonicalSideQueryRequest::Strict(prepared))
}

fn strict_request(
    analyst: &ResolvedPanel,
    user: String,
    schema: Value,
    output_tokens: u32,
    system_prompt: &str,
) -> StrictStructuredQueryRequest {
    StrictStructuredQueryRequest {
        model_attempt: None,
        model: analyst.model.clone(),
        profile: Some(analyst.profile.clone()),
        system_prompt: Some(system_prompt.to_string()),
        messages: vec![ConversationMessage::user(MessageId::new(), user)],
        schema,
        max_tokens: output_tokens,
        temperature: Some(0.0),
        query_source: QuerySource::FusionAnalyst,
        skip_system_prompt_prefix: true,
    }
}

fn estimate(
    client: &dyn SideQueryClient,
    request: CanonicalSideQueryRequest,
) -> Result<sidequery::SideQueryEstimate, PackingError> {
    client
        .estimate_request(request)
        .map_err(|error| PackingError::Estimate(sanitize_estimate_error(&error)))
}

fn fits(
    client: &dyn SideQueryClient,
    request: CanonicalSideQueryRequest,
    limits: ModelLimits,
    output_tokens: u32,
) -> Result<bool, PackingError> {
    let input_cap = usable_input_cap(limits, output_tokens)?;
    Ok(estimate(client, request)?.input_tokens <= input_cap)
}

fn usable_input_cap(limits: ModelLimits, output_tokens: u32) -> Result<u64, PackingError> {
    if output_tokens == 0 {
        return Err(PackingError::UnusableCapacity);
    }
    match limits.input_cap(output_tokens) {
        Some(0) => Err(PackingError::UnusableCapacity),
        Some(cap) => Ok(cap),
        None => Err(PackingError::UnknownCapacity),
    }
}

fn sanitize_estimate_error(error: &SideQueryError) -> String {
    match error {
        SideQueryError::Api(_) => "provider_estimator_error".into(),
        SideQueryError::InvalidResponse(_) => "invalid_estimator_response".into(),
        SideQueryError::StructuredOutputUnsupported => "structured_output_unsupported".into(),
        SideQueryError::Partial { .. } => "partial_estimator_response".into(),
    }
}

/// Full analyst payload, retained here as the canonical input to both the
/// full-request fast path and the packed fallback.
pub(crate) fn analyst_user_message(
    request: &FusionRequest,
    panels: &[PanelInternal],
    retry_hint: Option<&str>,
) -> String {
    let mut reports = Vec::new();
    for panel in sorted_reports(panels) {
        if let Some(report) = &panel.report {
            let mut report_value = json!(report);
            report_value["evidence"] = evidence_with_checks(panel, report);
            let entry = json!({
                "panel_id": panel.anonymous_id,
                "report": report_value,
            });
            reports.push(entry);
        }
    }
    let mut payload = json!({
        "task": request.prompt,
        "dimensions": request.dimensions,
        "panels": reports,
    });
    if let Some(hint) = retry_hint {
        let safe_hint = truncate_bytes(&sanitize_text(hint), RETRY_HINT_BYTE_CAP);
        payload["retry_reason"] = Value::String(format!(
            "Your previous response could not be used: {safe_hint}. Return ONLY JSON matching \
the schema, with no other text."
        ));
    }
    payload.to_string()
}

fn packed_analyst_user_message(
    request: &FusionRequest,
    retry_hint: Option<&str>,
    sources: &[PackedPanelSource],
    optional_budget: usize,
    omission_mode: OmissionMode,
) -> String {
    let quotas = hierarchical_fair_quotas(sources, optional_budget);
    let mut omitted = OmissionCounts::default();
    let mut quota_index = 0;
    let panel_values = sources
        .iter()
        .map(|source| {
            let mut value = json!({
                "panel_id": source.panel_id,
                "critical_risks": source.critical_risks,
            });
            let excerpts = source
                .optional_groups
                .iter()
                .map(|group| {
                    let quota = quotas.get(quota_index).copied().unwrap_or_default();
                    quota_index += 1;
                    excerpt_prefix(group, quota)
                })
                .collect::<Vec<_>>();
            insert_excerpt(&mut value, "summary", &excerpts[0].0);
            insert_excerpt(&mut value, "candidate_answer", &excerpts[1].0);
            insert_excerpt(&mut value, "claims_evidence_excerpt", &excerpts[2].0);
            insert_excerpt(
                &mut value,
                "risks_assumptions_questions_excerpt",
                &excerpts[3].0,
            );

            if !source.report_present {
                omitted.unavailable_panels = omitted.unavailable_panels.saturating_add(1);
            } else {
                let truncated = excerpts
                    .iter()
                    .zip(&source.optional_groups)
                    .map(|(excerpt, group)| excerpt.1 < group.len())
                    .collect::<Vec<_>>();
                if truncated.iter().any(|truncated| *truncated) {
                    omitted.panels = omitted.panels.saturating_add(1);
                }
                omitted.summary_bytes = omitted.summary_bytes.saturating_add(saturating_u64(
                    source.optional_groups[0]
                        .len()
                        .saturating_sub(excerpts[0].1),
                ));
                omitted.candidate_answer_bytes =
                    omitted
                        .candidate_answer_bytes
                        .saturating_add(saturating_u64(
                            source.optional_groups[1]
                                .len()
                                .saturating_sub(excerpts[1].1),
                        ));
                if truncated[2] {
                    omitted.claims = omitted.claims.saturating_add(source.counts.claims);
                    omitted.evidence = omitted.evidence.saturating_add(source.counts.evidence);
                }
                if truncated[3] {
                    omitted.assumptions = omitted
                        .assumptions
                        .saturating_add(source.counts.assumptions);
                    omitted.risks = omitted.risks.saturating_add(source.counts.risks);
                    omitted.unresolved_questions = omitted
                        .unresolved_questions
                        .saturating_add(source.counts.unresolved_questions);
                }
            }
            value
        })
        .collect::<Vec<_>>();
    if omission_mode == OmissionMode::Conservative {
        omitted = OmissionCounts::conservative();
    }
    let mut payload = json!({
        "task": request.prompt,
        "dimensions": request.dimensions,
        "panels": panel_values,
        "omitted_panels": omitted.panels,
        "unavailable_panels": omitted.unavailable_panels,
        "omitted_summary_bytes": omitted.summary_bytes,
        "omitted_candidate_answer_bytes": omitted.candidate_answer_bytes,
        "omitted_claims": omitted.claims,
        "omitted_evidence": omitted.evidence,
        "omitted_assumptions": omitted.assumptions,
        "omitted_risks": omitted.risks,
        "omitted_unresolved_questions": omitted.unresolved_questions,
    });
    if let Some(hint) = retry_hint {
        let safe_hint = truncate_bytes(&sanitize_text(hint), RETRY_HINT_BYTE_CAP);
        payload["retry_reason"] = Value::String(format!(
            "Your previous response could not be used: {safe_hint}. Return ONLY JSON matching \
the schema, with no other text."
        ));
    }
    payload.to_string()
}

/// A report's evidence as the analyst sees it: each item carries the host's
/// `check` once the evidence stage has run (see [`crate::evidence`]).
fn evidence_with_checks(panel: &PanelInternal, report: &PanelReport) -> Value {
    let mut evidence = json!(report.evidence);
    if let Some(items) = evidence.as_array_mut() {
        for (item, check) in items.iter_mut().zip(&panel.evidence_checks) {
            item["check"] = json!(check.label());
        }
    }
    evidence
}

fn sorted_reports(panels: &[PanelInternal]) -> Vec<&PanelInternal> {
    let mut sorted = panels
        .iter()
        .filter(|panel| panel.report.is_some())
        .collect::<Vec<_>>();
    sorted.sort_by(|a, b| a.anonymous_id.cmp(&b.anonymous_id));
    sorted
}

fn sorted_panels(panels: &[PanelInternal]) -> Vec<&PanelInternal> {
    let mut sorted = panels.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| a.anonymous_id.cmp(&b.anonymous_id));
    sorted
}

fn critical_risks(report: &PanelReport) -> Vec<String> {
    report
        .risks
        .iter()
        .filter(|risk| risk.severity == RiskSeverity::Critical)
        .map(|risk| risk.description.clone())
        .collect()
}

#[derive(Clone, Copy, Debug, Default)]
struct ReportCounts {
    claims: u64,
    evidence: u64,
    assumptions: u64,
    risks: u64,
    unresolved_questions: u64,
}

#[derive(Clone, Debug)]
struct PackedPanelSource {
    panel_id: String,
    critical_risks: Vec<String>,
    optional_groups: Vec<String>,
    report_present: bool,
    counts: ReportCounts,
}

fn analyst_packed_sources(panels: &[PanelInternal]) -> Vec<PackedPanelSource> {
    sorted_panels(panels)
        .into_iter()
        .map(|panel| {
            let Some(report) = panel.report.as_ref() else {
                return PackedPanelSource {
                    panel_id: panel.anonymous_id.clone(),
                    critical_risks: Vec::new(),
                    optional_groups: vec![String::new(); 4],
                    report_present: false,
                    counts: ReportCounts::default(),
                };
            };
            let claims_evidence = if report.claims.is_empty() && report.evidence.is_empty() {
                String::new()
            } else {
                json!({
                    "claims": report.claims,
                    "evidence": evidence_with_checks(panel, report),
                })
                .to_string()
            };
            let noncritical_risks = report
                .risks
                .iter()
                .filter(|risk| risk.severity != RiskSeverity::Critical)
                .collect::<Vec<_>>();
            let contextual = if noncritical_risks.is_empty()
                && report.assumptions.is_empty()
                && report.unresolved_questions.is_empty()
            {
                String::new()
            } else {
                json!({
                    "risks": noncritical_risks,
                    "assumptions": report.assumptions,
                    "unresolved_questions": report.unresolved_questions,
                })
                .to_string()
            };
            PackedPanelSource {
                panel_id: panel.anonymous_id.clone(),
                critical_risks: critical_risks(report),
                optional_groups: vec![
                    report.summary.clone(),
                    report.candidate_answer.clone(),
                    claims_evidence,
                    contextual,
                ],
                report_present: true,
                counts: ReportCounts {
                    claims: saturating_u64(report.claims.len()),
                    evidence: saturating_u64(report.evidence.len()),
                    assumptions: saturating_u64(report.assumptions.len()),
                    risks: saturating_u64(
                        report
                            .risks
                            .iter()
                            .filter(|risk| risk.severity != RiskSeverity::Critical)
                            .count(),
                    ),
                    unresolved_questions: saturating_u64(report.unresolved_questions.len()),
                },
            }
        })
        .collect()
}

fn optional_demands(sources: &[PackedPanelSource]) -> Vec<usize> {
    sources
        .iter()
        .flat_map(|source| source.optional_groups.iter().map(String::len))
        .collect()
}

fn total_optional_bytes(sources: &[PackedPanelSource]) -> usize {
    optional_demands(sources)
        .into_iter()
        .fold(0_usize, usize::saturating_add)
}

/// Split the optional budget equally among successful panels first, then
/// water-fill that panel's groups. A panel with four populated groups must not
/// receive four times the opportunity of a peer with one long candidate.
fn hierarchical_fair_quotas(sources: &[PackedPanelSource], budget: usize) -> Vec<usize> {
    let panel_demands = sources
        .iter()
        .map(|source| {
            if source.report_present {
                source
                    .optional_groups
                    .iter()
                    .map(String::len)
                    .fold(0_usize, usize::saturating_add)
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let panel_quotas = fair_quotas(&panel_demands, budget);
    sources
        .iter()
        .zip(panel_quotas)
        .flat_map(|(source, panel_quota)| {
            let group_demands = source
                .optional_groups
                .iter()
                .map(String::len)
                .collect::<Vec<_>>();
            fair_quotas(&group_demands, panel_quota)
        })
        .collect()
}

/// Deterministic max-min allocation. Every unsaturated group receives the
/// same byte level; short groups return their unused share for redistribution.
fn fair_quotas(demands: &[usize], budget: usize) -> Vec<usize> {
    let total = demands.iter().copied().fold(0_usize, usize::saturating_add);
    let budget = budget.min(total);
    let mut low = 0_usize;
    let mut high = demands.iter().copied().max().unwrap_or_default();
    while low < high {
        let level = low.saturating_add(high.saturating_sub(low).div_ceil(2));
        let used = demands.iter().fold(0_usize, |sum, demand| {
            sum.saturating_add((*demand).min(level))
        });
        if used <= budget {
            low = level;
        } else {
            high = level.saturating_sub(1);
        }
    }
    let mut quotas = demands
        .iter()
        .map(|demand| (*demand).min(low))
        .collect::<Vec<_>>();
    let used = quotas.iter().copied().fold(0_usize, usize::saturating_add);
    let mut remaining = budget.saturating_sub(used);
    for (quota, demand) in quotas.iter_mut().zip(demands) {
        if remaining == 0 {
            break;
        }
        if *quota < *demand {
            *quota += 1;
            remaining -= 1;
        }
    }
    quotas
}

fn excerpt_prefix(value: &str, cap: usize) -> (String, usize) {
    let mut end = cap.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), end)
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn insert_excerpt(value: &mut Value, name: &str, excerpt: &str) {
    if !excerpt.is_empty() {
        value[name] = Value::String(excerpt.to_string());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OmissionMode {
    Actual,
    /// Use fixed-width worst-case counters during fit search so request size is
    /// monotone as optional excerpts grow. The final actual counters are never
    /// larger than these sentinels.
    Conservative,
}

#[derive(Clone, Copy, Debug, Default)]
struct OmissionCounts {
    panels: u64,
    unavailable_panels: u64,
    summary_bytes: u64,
    candidate_answer_bytes: u64,
    claims: u64,
    evidence: u64,
    assumptions: u64,
    risks: u64,
    unresolved_questions: u64,
}

impl OmissionCounts {
    const fn conservative() -> Self {
        Self {
            panels: u64::MAX,
            unavailable_panels: u64::MAX,
            summary_bytes: u64::MAX,
            candidate_answer_bytes: u64::MAX,
            claims: u64::MAX,
            evidence: u64::MAX,
            assumptions: u64::MAX,
            risks: u64::MAX,
            unresolved_questions: u64::MAX,
        }
    }
}

fn sanitize_text(value: &str) -> String {
    sanitize_blocks(&[value.replace('\0', "")]).content.join("")
}

fn truncate_bytes(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_string();
    }
    const ELLIPSIS_BYTES: usize = "…".len();
    if cap < ELLIPSIS_BYTES {
        return String::new();
    }
    let mut end = cap - ELLIPSIS_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use platform_api::{FusionOrigin, FusionPreset, PanelRunStatus};

    struct DtoEstimator;

    #[async_trait]
    impl SideQueryClient for DtoEstimator {
        async fn query(
            &self,
            _request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, SideQueryError> {
            unreachable!("packing tests only estimate")
        }
    }

    fn request() -> FusionRequest {
        FusionRequest {
            schema_version: 1,
            origin: FusionOrigin::Slash,
            prompt: "task".into(),
            preset: FusionPreset::Quality,
            models: None,
            dimensions: vec!["coverage".into(), "safety".into()],
            partial_ok: true,
            max_panel: None,
            cross_provider: true,
            parent_profile: "anthropic".into(),
            parent_model: "claude-sonnet-5".into(),
        }
    }

    fn panel(id: &str, size: usize) -> PanelInternal {
        PanelInternal {
            index: 0,
            profile: "p".into(),
            model: "m".into(),
            anonymous_id: id.into(),
            status: PanelRunStatus::Completed,
            report: Some(PanelReport {
                schema_version: 1,
                summary: "s".into(),
                candidate_answer: "x".repeat(size),
                claims: vec![],
                evidence: vec![],
                assumptions: vec![],
                risks: vec![platform_api::PanelRisk {
                    severity: RiskSeverity::Critical,
                    description: "do not ignore".into(),
                }],
                unresolved_questions: vec![],
            }),
            duration_ms: 0,
            error_category: None,
            error_detail: None,
            usage: None,
            spawn_prompt: String::new(),
            evidence_checks: Vec::new(),
        }
    }

    fn panel_without_report(id: &str) -> PanelInternal {
        let mut panel = panel(id, 0);
        panel.status = PanelRunStatus::Failed;
        panel.report = None;
        panel
    }

    fn first_user_text(messages: &[ConversationMessage]) -> &str {
        let ConversationMessage::User { content, .. } = &messages[0] else {
            panic!("expected a user message")
        };
        content
            .iter()
            .find_map(|block| match block {
                protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("user text")
    }

    #[test]
    fn full_analyst_payload_is_unchanged_when_it_fits() {
        let request = request();
        let panels = vec![panel("P2", 4), panel("P1", 4)];
        let schema = json!({"type": "object"});
        let system = "judge".to_string();
        let expected = analyst_user_message(&request, &panels, None);
        let built = prepare_analyst_request(
            &DtoEstimator,
            &request,
            &ResolvedPanel {
                profile: "p".into(),
                model: "m".into(),
            },
            &panels,
            schema,
            system,
            None,
            128,
            crate::model_resolver::known_test_limits(),
        )
        .expect("small request fits");
        let actual = first_user_text(&built.messages);
        assert_eq!(actual, expected);
    }

    #[test]
    fn oversized_payload_keeps_sorted_ids_and_critical_risks_with_counts() {
        let request = request();
        let panels = vec![panel("P2", 5_000), panel("P1", 5_000)];
        let built = prepare_analyst_request(
            &DtoEstimator,
            &request,
            &ResolvedPanel {
                profile: "p".into(),
                model: "m".into(),
            },
            &panels,
            json!({"type": "object"}),
            "judge".into(),
            None,
            128,
            ModelLimits {
                // 2,000 - 128 output - 1,024 margin leaves 848 contextual
                // tokens, so the direct 300-token cap governs: mandatory
                // metadata fits while the two 5 KiB reports still require
                // deterministic packing.
                context_window_tokens: Some(2_000),
                max_input_tokens: Some(300),
                max_output_tokens: Some(128),
            },
        )
        .expect("mandatory placeholders fit");
        let text = first_user_text(&built.messages);
        let value: Value = serde_json::from_str(text).expect("packed json");
        assert_eq!(value["panels"][0]["panel_id"], "P1");
        assert_eq!(value["panels"][1]["panel_id"], "P2");
        assert_eq!(value["panels"][0]["critical_risks"][0], "do not ignore");
        assert!(value["omitted_panels"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(
            value["omitted_risks"], 0,
            "retained critical risks must not also be reported as omitted"
        );
    }

    #[test]
    fn packed_analyst_water_fills_groups_and_is_completion_order_independent() {
        let request = request();
        let mut p1 = panel("P1", 8_000);
        p1.report.as_mut().unwrap().summary = "a".repeat(8_000);
        let mut p2 = panel("P2", 8_000);
        p2.report.as_mut().unwrap().summary = "b".repeat(8_000);
        let failed = panel_without_report("P3");

        let first_sources = analyst_packed_sources(&[p2.clone(), failed.clone(), p1.clone()]);
        let second_sources = analyst_packed_sources(&[p1, p2, failed]);
        let first = packed_analyst_user_message(
            &request,
            None,
            &first_sources,
            4_000,
            OmissionMode::Actual,
        );
        let second = packed_analyst_user_message(
            &request,
            None,
            &second_sources,
            4_000,
            OmissionMode::Actual,
        );
        assert_eq!(
            first, second,
            "completion order must not affect packed bytes"
        );

        let value: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(value["panels"][0]["panel_id"], "P1");
        assert_eq!(value["panels"][1]["panel_id"], "P2");
        assert_eq!(value["panels"][2]["panel_id"], "P3");
        let p1_summary = value["panels"][0]["summary"].as_str().unwrap().len();
        let p2_summary = value["panels"][1]["summary"].as_str().unwrap().len();
        let p1_candidate = value["panels"][0]["candidate_answer"]
            .as_str()
            .unwrap()
            .len();
        let p2_candidate = value["panels"][1]["candidate_answer"]
            .as_str()
            .unwrap()
            .len();
        assert!(p1_summary.abs_diff(p2_summary) <= 1);
        assert!(p1_candidate.abs_diff(p2_candidate) <= 1);
        assert!(p1_summary > 0 && p2_summary > 0 && p1_candidate > 0 && p2_candidate > 0);
        assert_eq!(value["unavailable_panels"], 1);
    }

    #[test]
    fn fair_allocation_is_panel_first_when_group_counts_are_asymmetric() {
        let mut one_group = panel("P1", 8_000);
        one_group.report.as_mut().unwrap().summary.clear();

        let mut four_groups = panel("P2", 8_000);
        let report = four_groups.report.as_mut().unwrap();
        report.summary = "summary".repeat(1_000);
        report.claims.push(platform_api::PanelClaim {
            statement: "claim".repeat(1_000),
            evidence_refs: vec!["e1".into()],
            confidence: 80,
        });
        report.evidence.push(platform_api::PanelEvidence {
            id: "e1".into(),
            kind: platform_api::EvidenceKind::File,
            locator: "src/lib.rs".into(),
            excerpt: Some("evidence".repeat(1_000)),
        });
        report.assumptions.push("assumption".repeat(1_000));

        let sources = analyst_packed_sources(&[four_groups, panel_without_report("P3"), one_group]);
        let quotas = hierarchical_fair_quotas(&sources, 4_000);
        let p1_total: usize = quotas[0..4].iter().sum();
        let p2_total: usize = quotas[4..8].iter().sum();
        let failed_total: usize = quotas[8..12].iter().sum();
        assert!(
            p1_total.abs_diff(p2_total) <= 1,
            "successful panels get equal opportunity before their internal groups: {quotas:?}"
        );
        assert_eq!(failed_total, 0, "a failed panel has no optional share");
        assert!(quotas[4..8].iter().all(|quota| *quota > 0));
    }

    #[test]
    fn preflight_reserves_the_largest_retry_hint_before_first_dispatch() {
        let request = request();
        let analyst = ResolvedPanel {
            profile: "p".into(),
            model: "m".into(),
        };
        let schema = json!({"type": "object"});
        let system = "judge".to_string();
        let without_retry = strict_request(
            &analyst,
            analyst_user_message(&request, &[], None),
            schema.clone(),
            128,
            &system,
        );
        let cap = DtoEstimator
            .estimate_request(CanonicalSideQueryRequest::Strict(without_retry))
            .unwrap()
            .input_tokens;
        let limits = ModelLimits {
            context_window_tokens: None,
            max_input_tokens: Some(cap),
            max_output_tokens: Some(128),
        };
        preflight_analyst_request(
            &DtoEstimator,
            &request,
            &analyst,
            &[],
            schema.clone(),
            system.clone(),
            false,
            128,
            limits,
        )
        .expect("the first request itself fits");
        assert!(matches!(
            preflight_analyst_request(
                &DtoEstimator,
                &request,
                &analyst,
                &[],
                schema,
                system,
                true,
                128,
                limits,
            ),
            Err(PackingError::MandatoryTooLarge { .. })
        ));
    }

    #[test]
    fn eight_panels_and_twelve_dimensions_pack_to_the_exact_final_cap() {
        let mut request = request();
        request.dimensions = (1..=12).map(|index| format!("dimension_{index}")).collect();
        let panels = (1..=8)
            .rev()
            .map(|index| panel(&format!("P{index}"), 10_000))
            .collect::<Vec<_>>();
        let limits = ModelLimits {
            context_window_tokens: None,
            max_input_tokens: Some(2_000),
            max_output_tokens: Some(128),
        };
        let prepared = prepare_analyst_request(
            &DtoEstimator,
            &request,
            &ResolvedPanel {
                profile: "p".into(),
                model: "m".into(),
            },
            &panels,
            json!({"type": "object"}),
            "judge".into(),
            None,
            128,
            limits,
        )
        .expect("mandatory metadata fits and optional reports are packed");
        let final_estimate = DtoEstimator
            .estimate_request(CanonicalSideQueryRequest::Strict(prepared.clone()))
            .unwrap();
        assert!(final_estimate.input_tokens <= 2_000);
        let value: Value = serde_json::from_str(first_user_text(&prepared.messages)).unwrap();
        assert_eq!(value["dimensions"].as_array().unwrap().len(), 12);
        assert_eq!(value["panels"].as_array().unwrap().len(), 8);
        assert_eq!(value["panels"][0]["panel_id"], "P1");
        assert_eq!(value["panels"][7]["panel_id"], "P8");
    }

    #[test]
    fn unknown_capacity_fails_before_a_request_is_accepted() {
        let error = prepare_analyst_request(
            &DtoEstimator,
            &request(),
            &ResolvedPanel {
                profile: "p".into(),
                model: "m".into(),
            },
            &[panel("P1", 1)],
            json!({"type": "object"}),
            "judge".into(),
            None,
            128,
            ModelLimits::unknown(),
        )
        .expect_err("unknown capacity must fail closed");
        assert_eq!(error, PackingError::UnknownCapacity);
    }

    #[test]
    fn zero_output_capacity_never_builds_a_paid_judge_request() {
        let error = prepare_analyst_request(
            &DtoEstimator,
            &request(),
            &ResolvedPanel {
                profile: "p".into(),
                model: "m".into(),
            },
            &[panel("P1", 1)],
            json!({"type": "object"}),
            "judge".into(),
            None,
            0,
            ModelLimits {
                context_window_tokens: Some(16_000),
                max_input_tokens: Some(12_000),
                max_output_tokens: Some(0),
            },
        )
        .expect_err("max_tokens=0 must be rejected before any judge dispatch");
        assert_eq!(error, PackingError::UnusableCapacity);
    }

    #[test]
    fn mandatory_task_overflow_is_rejected_without_dropping_metadata() {
        let mut request = request();
        request.prompt = "界".repeat(2_000);
        let error = prepare_analyst_request(
            &DtoEstimator,
            &request,
            &ResolvedPanel {
                profile: "p".into(),
                model: "m".into(),
            },
            &[panel("P1", 1)],
            json!({"type": "object"}),
            "judge".into(),
            None,
            128,
            ModelLimits {
                context_window_tokens: None,
                max_input_tokens: Some(32),
                max_output_tokens: Some(128),
            },
        )
        .expect_err("mandatory CJK task must fail closed when it cannot fit");
        assert!(matches!(error, PackingError::MandatoryTooLarge { .. }));
    }

    #[test]
    fn retry_hint_truncation_is_utf8_safe_and_never_exceeds_its_total_cap() {
        let value = format!("{}{}", "界".repeat(RETRY_HINT_BYTE_CAP), "\\\"");
        let truncated = truncate_bytes(&value, RETRY_HINT_BYTE_CAP);
        assert!(truncated.len() <= RETRY_HINT_BYTE_CAP);
        assert!(truncated.ends_with('…'));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }
}
