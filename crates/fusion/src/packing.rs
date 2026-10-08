//! Deterministic request construction and route-capacity packing for judge calls.
//!
//! The full request is always tried first.  Only when the canonical request
//! estimate does not fit do we replace optional report material with stable
//! anonymous-id placeholders.  The task, dimensions, every panel id, and
//! every critical risk remain in the packed form; omission counters make the
//! loss explicit to the judge and to tests.

use crate::model_resolver::{ModelLimits, ResolvedPanel};
use crate::panel::PanelInternal;
use lingxi_core::host::subagent_output_guard::sanitize_blocks;
use lingxi_core::host::{
    truncate_tail, FusionRequest, PanelPatch, PanelReport, PanelVerification, RiskSeverity,
    VerificationOutcome,
};
use lingxi_core::types::{ConversationMessage, MessageId};
use serde_json::{json, Value};
use sidequery::{
    CanonicalSideQueryRequest, QuerySource, SideQueryClient, SideQueryError,
    StrictStructuredQueryRequest,
};
const RETRY_HINT_BYTE_CAP: usize = 512;
/// Diff bytes shared by every implement-mode panel in the analyst's full
/// request. Each panel's own diff is already capped at
/// [`lingxi_core::host::FUSION_MATERIAL_DIFF_BYTE_CAP`]; this keeps N panels from
/// multiplying it.
const ANALYST_DIFF_TOTAL_BYTE_CAP: usize = 48 * 1024;
/// Changed files listed per panel in the analyst's full request.
const ANALYST_MAX_PATCH_FILES: usize = 32;
/// Longest verification command echoed to the analyst.
const ANALYST_COMMAND_BYTE_CAP: usize = 500;
/// Output kept from each verification run that did not pass, full request.
const ANALYST_OUTPUT_TAIL_BYTE_CAP: usize = 2 * 1024;
/// Optional groups per panel: summary, candidate answer, claims and evidence,
/// risks and assumptions — and, in implement mode, the patch and the output of
/// the verification runs that did not pass.
const ANALYSIS_GROUPS: usize = 4;
const IMPLEMENT_GROUPS: usize = 6;

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
    let diff_quotas = diff_quotas(&sorted_reports(panels));
    for (panel, diff_quota) in sorted_reports(panels).into_iter().zip(diff_quotas) {
        if let Some(report) = &panel.report {
            let mut report_value = json!(report);
            report_value["evidence"] = evidence_with_checks(panel, report);
            let mut entry = json!({
                "panel_id": panel.anonymous_id,
                "report": report_value,
            });
            if panel.implement.worktree.is_some() {
                entry["implement"] = implement_value(panel, diff_quota);
            }
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
            if let Some(implement) = &source.implement {
                value["implement"] = implement.clone();
                insert_excerpt(&mut value, "patch_excerpt", &excerpts[4].0);
                insert_excerpt(&mut value, "verification_output_excerpt", &excerpts[5].0);
            }

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
                if source.implement.is_some() {
                    omitted.patch_bytes = omitted.patch_bytes.saturating_add(saturating_u64(
                        source.optional_groups[4]
                            .len()
                            .saturating_sub(excerpts[4].1),
                    ));
                    omitted.verification_output_bytes = omitted
                        .verification_output_bytes
                        .saturating_add(saturating_u64(
                            source.optional_groups[5]
                                .len()
                                .saturating_sub(excerpts[5].1),
                        ));
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
    if sources.iter().any(|source| source.implement.is_some()) {
        payload["omitted_patch_bytes"] = json!(omitted.patch_bytes);
        payload["omitted_verification_output_bytes"] = json!(omitted.verification_output_bytes);
    }
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

/// Each report panel's share of [`ANALYST_DIFF_TOTAL_BYTE_CAP`], in the given
/// order: a max-min split, so a short diff returns its unused share.
fn diff_quotas(reports: &[&PanelInternal]) -> Vec<usize> {
    let demands = reports
        .iter()
        .map(|panel| {
            panel
                .implement
                .patch
                .as_ref()
                .map_or(0, |patch| patch.diff.len())
        })
        .collect::<Vec<_>>();
    fair_quotas(&demands, ANALYST_DIFF_TOTAL_BYTE_CAP)
}

/// What the analyst always sees of an implement-mode panel: how big the
/// change is and how the host's verification commands came out. Everything
/// here is a host fact or bounded; the diff and the output tails are added
/// by [`implement_value`] or carried as optional groups.
fn implement_summary(panel: &PanelInternal) -> Value {
    let patch = match &panel.implement.patch {
        Some(patch) if patch.is_empty() => json!({ "changed": false }),
        Some(patch) => json!({
            "changed": true,
            "files_changed": patch.files.len().saturating_add(patch.files_omitted),
            "insertions": patch.insertions,
            "deletions": patch.deletions,
        }),
        None => json!({ "unavailable": true }),
    };
    let mut value = json!({ "patch": patch });
    match &panel.implement.verification {
        None => {}
        Some(PanelVerification::NotConfigured) => {
            value["verification"] = json!("not_configured");
        }
        Some(verification @ PanelVerification::Runs(runs)) => {
            let runs = runs
                .iter()
                .map(|run| {
                    let mut item = json!({
                        "command": truncate_bytes(&sanitize_text(&run.command), ANALYST_COMMAND_BYTE_CAP),
                        "outcome": run.outcome.label(),
                        "duration_ms": run.duration_ms,
                    });
                    if let VerificationOutcome::Failed {
                        exit_code: Some(code),
                    } = &run.outcome
                    {
                        item["exit_code"] = json!(code);
                    }
                    item
                })
                .collect::<Vec<_>>();
            value["verification"] = json!({
                "summary": verification.summary(),
                "runs": runs,
            });
        }
    }
    value
}

/// The changed files, one per line, then the diff: the text the packed
/// request cuts by prefix, so the file list outlasts the diff.
fn patch_text(panel: &PanelInternal) -> String {
    let Some(patch) = panel.implement.patch.as_ref().filter(|p| !p.is_empty()) else {
        return String::new();
    };
    let mut text = patch_file_lines(patch, usize::MAX);
    if !patch.diff.is_empty() {
        text.push('\n');
        text.push_str(&sanitize_text(&patch.diff));
    }
    text
}

fn patch_file_lines(patch: &PanelPatch, max_files: usize) -> String {
    let mut lines = patch
        .files
        .iter()
        .take(max_files)
        .map(|file| {
            format!(
                "{} {} (+{} -{})",
                file.status.label(),
                truncate_bytes(&sanitize_text(&file.path), ANALYST_COMMAND_BYTE_CAP),
                file.insertions,
                file.deletions
            )
        })
        .collect::<Vec<_>>();
    let omitted = patch
        .files
        .len()
        .saturating_sub(max_files)
        .saturating_add(patch.files_omitted);
    if omitted > 0 {
        lines.push(format!("… and {omitted} more files"));
    }
    lines.join("\n")
}

/// The output of every verification run that did not pass, each under its
/// command, tail kept.
fn failed_output_text(panel: &PanelInternal) -> String {
    let Some(PanelVerification::Runs(runs)) = &panel.implement.verification else {
        return String::new();
    };
    runs.iter()
        .filter(|run| run.outcome != VerificationOutcome::Passed && !run.output_tail.is_empty())
        .map(|run| {
            format!(
                "$ {}\n{}",
                truncate_bytes(&sanitize_text(&run.command), ANALYST_COMMAND_BYTE_CAP),
                sanitize_text(&run.output_tail)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// An implement-mode panel as the full analyst request carries it:
/// [`implement_summary`] plus the changed files, the diff cut to
/// `diff_quota` bytes, and the tail of each run that did not pass.
fn implement_value(panel: &PanelInternal, diff_quota: usize) -> Value {
    let mut value = implement_summary(panel);
    if let Some(patch) = panel.implement.patch.as_ref().filter(|p| !p.is_empty()) {
        let (diff, kept) = excerpt_prefix(&patch.diff, diff_quota);
        value["patch"]["files"] = Value::String(patch_file_lines(patch, ANALYST_MAX_PATCH_FILES));
        value["patch"]["diff"] = Value::String(sanitize_text(&diff));
        if patch.diff_truncated || kept < patch.diff.len() {
            value["patch"]["diff_truncated"] = Value::Bool(true);
        }
    }
    if let (Some(PanelVerification::Runs(runs)), Some(items)) = (
        &panel.implement.verification,
        value
            .get_mut("verification")
            .and_then(|verification| verification.get_mut("runs"))
            .and_then(Value::as_array_mut),
    ) {
        for (item, run) in items.iter_mut().zip(runs) {
            if run.outcome != VerificationOutcome::Passed && !run.output_tail.is_empty() {
                item["output_tail"] = Value::String(sanitize_text(&truncate_tail(
                    &run.output_tail,
                    ANALYST_OUTPUT_TAIL_BYTE_CAP,
                )));
            }
        }
    }
    value
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
    /// [`ANALYSIS_GROUPS`] groups, or [`IMPLEMENT_GROUPS`] with `implement`.
    optional_groups: Vec<String>,
    report_present: bool,
    counts: ReportCounts,
    /// Implement mode: the change's size and the verification outcomes, which
    /// always stay in the packed request; the diff and the output of failed
    /// runs are optional groups.
    implement: Option<Value>,
}

fn analyst_packed_sources(panels: &[PanelInternal]) -> Vec<PackedPanelSource> {
    sorted_panels(panels)
        .into_iter()
        .map(|panel| {
            let Some(report) = panel.report.as_ref() else {
                return PackedPanelSource {
                    panel_id: panel.anonymous_id.clone(),
                    critical_risks: Vec::new(),
                    optional_groups: vec![String::new(); ANALYSIS_GROUPS],
                    report_present: false,
                    counts: ReportCounts::default(),
                    implement: None,
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
            let mut optional_groups = vec![
                report.summary.clone(),
                report.candidate_answer.clone(),
                claims_evidence,
                contextual,
            ];
            let implement = panel.implement.worktree.is_some().then(|| {
                optional_groups.push(patch_text(panel));
                optional_groups.push(failed_output_text(panel));
                implement_summary(panel)
            });
            debug_assert_eq!(
                optional_groups.len(),
                if implement.is_some() {
                    IMPLEMENT_GROUPS
                } else {
                    ANALYSIS_GROUPS
                }
            );
            PackedPanelSource {
                panel_id: panel.anonymous_id.clone(),
                critical_risks: critical_risks(report),
                optional_groups,
                implement,
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
    patch_bytes: u64,
    verification_output_bytes: u64,
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
            patch_bytes: u64::MAX,
            verification_output_bytes: u64::MAX,
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
    use lingxi_core::host::{
        FusionOrigin, FusionPanelMode, FusionPreset, PanelRunStatus, PatchFile, PatchFileStatus,
        VerificationRun, WorktreeHandle,
    };

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
            verify_claims: false,
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
            mode: Default::default(),
            verify_commands: Vec::new(),
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
                risks: vec![lingxi_core::host::PanelRisk {
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
            implement: Default::default(),
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
                lingxi_core::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
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
        report.claims.push(lingxi_core::host::PanelClaim {
            statement: "claim".repeat(1_000),
            evidence_refs: vec!["e1".into()],
            confidence: 80,
        });
        report.evidence.push(lingxi_core::host::PanelEvidence {
            id: "e1".into(),
            kind: lingxi_core::host::EvidenceKind::File,
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

    fn implement_request() -> FusionRequest {
        FusionRequest {
            mode: FusionPanelMode::Implement,
            dimensions: vec!["correctness".into(), "scope".into()],
            ..request()
        }
    }

    /// An implement-mode panel whose worktree holds a `diff_bytes` diff, with
    /// one passing and one failing verification run.
    fn implement_panel(id: &str, diff_bytes: usize) -> PanelInternal {
        let mut panel = panel(id, 10);
        panel.implement.worktree = Some(WorktreeHandle {
            path: format!("/wt/fusion-x-{id}").into(),
            branch_name: format!("worktree-fusion-x-{id}"),
            base_commit: None,
        });
        panel.implement.patch = Some(PanelPatch {
            worktree: format!("/wt/fusion-x-{id}"),
            branch: format!("worktree-fusion-x-{id}"),
            base_commit: "abc1234".into(),
            patch_file: Some(format!("/wt/fusion-x-{id}.patch")),
            files: vec![PatchFile {
                path: "src/a.rs".into(),
                status: PatchFileStatus::Modified,
                insertions: 3,
                deletions: 1,
                binary: false,
            }],
            files_omitted: 0,
            insertions: 3,
            deletions: 1,
            diff: "d".repeat(diff_bytes),
            diff_truncated: false,
        });
        panel.implement.verification = Some(PanelVerification::Runs(vec![
            VerificationRun {
                command: "cargo check".into(),
                outcome: VerificationOutcome::Passed,
                duration_ms: 10,
                output_tail: "Finished".into(),
            },
            VerificationRun {
                command: "cargo test".into(),
                outcome: VerificationOutcome::Failed {
                    exit_code: Some(101),
                },
                duration_ms: 20,
                output_tail: "test result: FAILED. 1 failed".into(),
            },
        ]));
        panel
    }

    #[test]
    fn implement_panels_carry_patch_stats_and_verification_facts_in_the_full_payload() {
        let request = implement_request();
        let unchanged = {
            let mut panel = implement_panel("P2", 0);
            panel.implement.patch = Some(PanelPatch::default());
            panel.implement.verification = None;
            panel
        };
        let unavailable = {
            let mut panel = implement_panel("P3", 0);
            panel.implement.patch = None;
            panel.implement.patch_error = Some("git failed".into());
            panel.implement.verification = Some(PanelVerification::NotConfigured);
            panel
        };
        let panels = vec![unavailable, unchanged, implement_panel("P1", 40)];
        let value: Value = serde_json::from_str(&analyst_user_message(&request, &panels, None))
            .expect("full payload is json");

        let changed = &value["panels"][0]["implement"];
        assert_eq!(value["panels"][0]["panel_id"], "P1");
        assert_eq!(changed["patch"]["changed"], true);
        assert_eq!(changed["patch"]["files_changed"], 1);
        assert_eq!(changed["patch"]["insertions"], 3);
        assert_eq!(changed["patch"]["deletions"], 1);
        assert_eq!(changed["patch"]["files"], "modified src/a.rs (+3 -1)");
        assert_eq!(changed["patch"]["diff"], "d".repeat(40));
        assert!(changed["patch"].get("diff_truncated").is_none());
        assert_eq!(changed["verification"]["summary"], "1/2 passed");
        let runs = &changed["verification"]["runs"];
        assert_eq!(runs[0]["outcome"], "passed");
        assert!(
            runs[0].get("output_tail").is_none(),
            "a passing run's output is noise"
        );
        assert_eq!(runs[1]["outcome"], "failed");
        assert_eq!(runs[1]["exit_code"], 101);
        assert_eq!(runs[1]["output_tail"], "test result: FAILED. 1 failed");

        // No changes: nothing to verify, and no diff.
        let empty = &value["panels"][1]["implement"];
        assert_eq!(empty["patch"], json!({ "changed": false }));
        assert!(empty.get("verification").is_none());

        // Not collected, and no verification configured.
        let missing = &value["panels"][2]["implement"];
        assert_eq!(missing["patch"], json!({ "unavailable": true }));
        assert_eq!(missing["verification"], "not_configured");
        assert!(
            !value.to_string().contains("git failed"),
            "the collection error stays with the host"
        );
    }

    #[test]
    fn analysis_panels_carry_no_implement_keys_even_in_a_packed_payload() {
        let request = request();
        let panels = vec![panel("P1", 5_000), panel("P2", 5_000)];
        let full: Value =
            serde_json::from_str(&analyst_user_message(&request, &panels, None)).unwrap();
        assert!(full["panels"][0].get("implement").is_none());
        let sources = analyst_packed_sources(&panels);
        assert!(sources
            .iter()
            .all(|source| source.optional_groups.len() == 4));
        let packed: Value = serde_json::from_str(&packed_analyst_user_message(
            &request,
            None,
            &sources,
            100,
            OmissionMode::Actual,
        ))
        .unwrap();
        assert!(packed["panels"][0].get("implement").is_none());
        assert!(packed.get("omitted_patch_bytes").is_none());
        assert!(packed.get("omitted_verification_output_bytes").is_none());
    }

    #[test]
    fn implement_diffs_share_one_budget_and_a_short_diff_returns_its_share() {
        let request = implement_request();
        let panels = vec![
            implement_panel("P1", 40 * 1024),
            implement_panel("P2", 40 * 1024),
            implement_panel("P3", 1_000),
        ];
        let value: Value =
            serde_json::from_str(&analyst_user_message(&request, &panels, None)).unwrap();
        let diff = |index: usize| {
            value["panels"][index]["implement"]["patch"]["diff"]
                .as_str()
                .unwrap()
                .len()
        };
        assert_eq!(diff(2), 1_000, "a short diff is not cut");
        assert!(value["panels"][2]["implement"]["patch"]
            .get("diff_truncated")
            .is_none());
        assert!(
            diff(0).abs_diff(diff(1)) <= 1,
            "equal demands share equally"
        );
        assert_eq!(diff(0) + diff(1) + diff(2), ANALYST_DIFF_TOTAL_BYTE_CAP);
        assert_eq!(
            value["panels"][0]["implement"]["patch"]["diff_truncated"],
            true
        );
    }

    #[test]
    fn a_diff_the_host_already_cut_is_still_marked_truncated() {
        let request = implement_request();
        let mut panel = implement_panel("P1", 100);
        panel.implement.patch.as_mut().unwrap().diff_truncated = true;
        let value: Value =
            serde_json::from_str(&analyst_user_message(&request, &[panel], None)).unwrap();
        assert_eq!(
            value["panels"][0]["implement"]["patch"]["diff_truncated"],
            true
        );
    }

    #[test]
    fn long_verification_output_and_commands_are_capped_in_the_full_payload() {
        let request = implement_request();
        let mut panel = implement_panel("P1", 10);
        let Some(PanelVerification::Runs(runs)) = panel.implement.verification.as_mut() else {
            unreachable!()
        };
        runs[1].output_tail = format!("{}\nthe end", "x".repeat(10_000));
        runs[1].command = "c".repeat(2_000);
        let value: Value =
            serde_json::from_str(&analyst_user_message(&request, &[panel], None)).unwrap();
        let run = &value["panels"][0]["implement"]["verification"]["runs"][1];
        let tail = run["output_tail"].as_str().unwrap();
        assert!(tail.len() <= ANALYST_OUTPUT_TAIL_BYTE_CAP + "[truncated]…".len());
        assert!(tail.ends_with("the end"), "the tail keeps the end");
        assert!(run["command"].as_str().unwrap().len() <= ANALYST_COMMAND_BYTE_CAP);
    }

    #[test]
    fn a_packed_implement_payload_keeps_the_facts_and_counts_the_omitted_bytes() {
        let request = implement_request();
        let panels = vec![
            implement_panel("P2", 20_000),
            implement_panel("P1", 20_000),
            panel_without_report("P3"),
        ];
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
                context_window_tokens: None,
                max_input_tokens: Some(900),
                max_output_tokens: Some(128),
            },
        )
        .expect("the patch stats and verification outcomes fit");
        let value: Value = serde_json::from_str(first_user_text(&built.messages)).unwrap();
        for index in 0..2 {
            let implement = &value["panels"][index]["implement"];
            assert_eq!(implement["patch"]["changed"], true, "P{}", index + 1);
            assert_eq!(implement["patch"]["files_changed"], 1);
            assert_eq!(implement["verification"]["summary"], "1/2 passed");
            assert_eq!(implement["verification"]["runs"][1]["exit_code"], 101);
        }
        assert!(
            value["omitted_patch_bytes"].as_u64().unwrap() > 0,
            "the diffs cannot all fit"
        );
        assert_eq!(value["unavailable_panels"], 1);
        assert!(
            value["panels"][2].get("implement").is_none(),
            "an unavailable panel has no patch to show"
        );
    }

    #[test]
    fn the_packed_patch_text_lists_files_before_the_diff_so_the_list_outlasts_it() {
        let panel = implement_panel("P1", 500);
        let text = patch_text(&panel);
        assert!(text.starts_with("modified src/a.rs (+3 -1)\n"));
        assert!(text.ends_with(&"d".repeat(500)));
        let sources = analyst_packed_sources(&[panel]);
        assert_eq!(sources[0].optional_groups.len(), IMPLEMENT_GROUPS);
        assert!(sources[0].optional_groups[5].starts_with("$ cargo test\n"));
        assert!(
            !sources[0].optional_groups[5].contains("cargo check"),
            "only runs that did not pass keep their output"
        );
        let packed: Value = serde_json::from_str(&packed_analyst_user_message(
            &implement_request(),
            None,
            &sources,
            90,
            OmissionMode::Actual,
        ))
        .unwrap();
        let excerpt = packed["panels"][0]["patch_excerpt"].as_str().unwrap();
        assert!(excerpt.len() <= 90);
        assert!(
            excerpt.starts_with("modified src/a.rs"),
            "a short budget keeps the file list: {excerpt}"
        );
    }
}
