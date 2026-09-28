//! Host-owned record of a plan the USER approved in a conversation.
//!
//! `LocalAppPrepare` must refuse to land a template on the strength of a plan
//! the model merely *claims* was approved. The engine has exactly one writer of
//! "this plan is approved": the success branch of `ExitPlanMode`
//! (`tools/plan/src/plan_mode.rs`), which only runs after
//! `PermissionGate::check_exit_plan_mode` answered `Allow`. That branch returns
//! `{plan, filePath, isAgent, …}`, the adapter lowers it to
//! `ClientEvent::ToolUseResult`, and the turn loop delivers it to the registered
//! listener.
//!
//! So this module observes the approval as a listener decorator rather than as a
//! permission-gate decorator: [`crate::mobile::local_apps_host`] consumes a validated
//! template selection, and the `PermissionGate` trait carries 22 methods whose
//! transport/policy overrides a delegating newtype would have to forward
//! verbatim (a missed forward silently degrades permission enforcement). The
//! listener has one required method and one defaulted one, and the observer is
//! transparent — it records, then always forwards.
//!
//! The record is keyed by plan-file path and bound to the conversation that
//! approved it, so an approval obtained in one app's conversation can never be
//! spent in another.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use client::adapter::ClientEventListener;
use client::protocol::events::ClientEvent;
use local_apps::{sha256_hex, AppAuthoringSpec};
use serde::Deserialize;

/// Fenced-block info string a Local App plan carries its machine-readable
/// authoring block under. Every other fence in the plan is ignored.
pub(crate) const AUTHORING_SPEC_FENCE: &str = "authoring-spec";

/// Which authority answered the create confirmation a sealing sequence is
/// about to record.
///
/// The tool path only ever passes [`Self::NativeSheet`]; [`Self::ApprovedPlan`]
/// is reachable solely from Host code that already holds a user decision, so
/// the authority can never be selected by model input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateApprovalAuthority {
    /// Ask the user through the native create confirmation sheet.
    NativeSheet,
    /// The user's approval of the plan this create was derived from IS the
    /// answer; asking again is the duplicate confirmation the plan flow exists
    /// to remove.
    ApprovedPlan,
}

/// The machine-readable block embedded in a Local App plan.
///
/// `deny_unknown_fields` (plus [`AppAuthoringSpec`]'s own) keeps the block
/// closed: a plan that carries extra keys the Host does not understand is
/// rejected rather than silently half-honoured.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct PlanAuthoringBlock {
    /// The app's display name. Carried HERE rather than taken from the
    /// `LocalAppPrepare` call so the name the user reads in the plan IS the
    /// name the scaffold commits — the native create sheet that used to be
    /// the check for that is the confirmation this flow removes.
    name: String,
    /// The app's one-line brief, for the same reason as `name`.
    brief: String,
    /// The template the plan wants. Required to CREATE (there is nothing to
    /// land otherwise) and refused for MODIFY, whose runtime profile the app
    /// already fixed when it was scaffolded.
    #[serde(default)]
    template_id: Option<String>,
    spec: AppAuthoringSpec,
}

/// One accepted plan approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanApproval {
    /// The conversation the approval was observed in.
    pub(crate) session_uuid: String,
    /// Absolute plan-file path the engine reported for the approval.
    pub(crate) plan_path: String,
    /// Digest of the approved plan text, so a later edit cannot be spent.
    pub(crate) plan_sha256: String,
    /// The app's display name, as approved in the plan.
    pub(crate) name: String,
    /// The app's brief, as approved in the plan.
    pub(crate) brief: String,
    /// The template the approved plan selected, when it named one.
    pub(crate) template_id: Option<String>,
    /// The authoring spec the approved plan carries.
    pub(crate) spec: AppAuthoringSpec,
    /// The one app that has spent this approval, if any.
    ///
    /// One approval lands ONE app's template. Without this, a conversation
    /// holding two empty shells could land the same approved plan on the
    /// second one after the first prepare — the "cross-app reference" the
    /// Host has to refuse. Retrying the SAME app is not a second spend, so a
    /// failed prepare stays retryable without asking the user to plan again.
    spent_by: Option<String>,
}

/// What was observed for one plan path. A `Rejected` slot is kept so a
/// malformed Local App plan can be reported by name instead of looking like the
/// user never planned at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanApprovalSlot {
    Approved(PlanApproval),
    Rejected {
        session_uuid: String,
        reason: String,
    },
}

/// Per-plan-path slot for the most recently observed plan exit.
#[derive(Default)]
pub(crate) struct PlanApprovalLog {
    slots: Mutex<HashMap<String, PlanApprovalSlot>>,
}

impl PlanApprovalLog {
    /// Record the outcome of one observed `ExitPlanMode` success.
    pub(crate) fn record(&self, plan_path: String, slot: PlanApprovalSlot) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.insert(plan_path, slot);
        }
    }

    /// Spend the approval for `plan_path` on one app.
    ///
    /// The spend is recorded so the SAME approval cannot then land a second
    /// app's template, and it is recorded PER APP so a retry after a failed
    /// prepare is not mistaken for a second spend. Every rejection names the
    /// remedy, because the caller's only correct response is to send the user
    /// back through planning.
    pub(crate) fn claim(
        &self,
        plan_path: &str,
        session_uuid: &str,
        app_id: &str,
    ) -> Result<PlanApproval, String> {
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| "plan_approval_unavailable: the plan record is poisoned".to_string())?;
        let foreign = || {
            "plan_approval_missing: no plan approved in THIS conversation names that plan file; \
             plan the app here and let the user approve it"
                .to_string()
        };
        let slot = slots.get_mut(plan_path).ok_or_else(foreign)?;
        match slot {
            PlanApprovalSlot::Rejected {
                session_uuid: recorded,
                reason,
            } if recorded == session_uuid => Err(format!("plan_approval_invalid: {reason}")),
            PlanApprovalSlot::Rejected { .. } => Err(foreign()),
            PlanApprovalSlot::Approved(approval) => {
                if approval.session_uuid != session_uuid {
                    return Err(foreign());
                }
                match approval.spent_by.as_deref() {
                    None => {
                        approval.spent_by = Some(app_id.to_string());
                        Ok(approval.clone())
                    }
                    Some(spent) if spent == app_id => Ok(approval.clone()),
                    Some(_) => Err(
                        "plan_approval_spent: this approved plan already prepared a different \
                         app; plan this one and let the user approve it"
                            .into(),
                    ),
                }
            }
        }
    }
}

/// Transparent listener that records every approved `ExitPlanMode`.
pub(crate) struct PlanApprovalWatcher {
    inner: Arc<dyn ClientEventListener>,
    log: Arc<PlanApprovalLog>,
    session_uuid: Arc<Mutex<String>>,
}

impl PlanApprovalWatcher {
    pub(crate) fn new(
        inner: Arc<dyn ClientEventListener>,
        log: Arc<PlanApprovalLog>,
        session_uuid: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            inner,
            log,
            session_uuid,
        }
    }

    /// Record `result_json` as an approval (or a rejection) for this process.
    fn observe(&self, result_json: &str) {
        let uuid = self
            .session_uuid
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let Some((plan_path, slot)) = parse_plan_result(result_json, &uuid) else {
            return;
        };
        self.log.record(plan_path, slot);
    }
}

#[async_trait]
impl ClientEventListener for PlanApprovalWatcher {
    async fn on_event(&self, event: ClientEvent) {
        if let ClientEvent::ToolUseResult {
            tool,
            result_json,
            is_error,
            ..
        } = &event
        {
            // `is_error` false is the engine's own statement that the tool
            // succeeded; a denied `ExitPlanMode` returns an error payload, so it
            // never reaches `observe`.
            if tool == "ExitPlanMode" && !*is_error {
                self.observe(result_json);
            }
        }
        self.inner.on_event(event).await;
    }

    async fn on_workflow_progress(
        &self,
        origin_session_id: String,
        task_id: String,
        run_id: String,
        progress: client::protocol::listings::WorkflowProgressDto,
    ) {
        self.inner
            .on_workflow_progress(origin_session_id, task_id, run_id, progress)
            .await;
    }
}

/// Read a plan-approval outcome out of an `ExitPlanMode` result payload.
///
/// Returns `None` when the payload is not a main-session plan exit we can bind
/// (a subagent plan, or a payload with no plan-file identity), which is not an
/// error — most plans in the product are not Local App plans.
fn parse_plan_result(result_json: &str, session_uuid: &str) -> Option<(String, PlanApprovalSlot)> {
    let value: serde_json::Value = serde_json::from_str(result_json).ok()?;
    // A subagent's plan is approved by its parent, not by the user.
    if value.get("isAgent").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    // Without a plan-file path there is nothing to re-verify against later, so
    // the approval cannot be safely spent: fail closed by recording nothing.
    let plan_path = value
        .get("filePath")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let plan = value
        .get("plan")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let slot = match parse_authoring_block(plan) {
        Ok(None) => return None,
        Ok(Some(block)) => PlanApprovalSlot::Approved(PlanApproval {
            session_uuid: session_uuid.to_string(),
            plan_path: plan_path.clone(),
            plan_sha256: sha256_hex(plan.as_bytes()),
            name: block.name,
            brief: block.brief,
            template_id: block.template_id,
            spec: block.spec,
            spent_by: None,
        }),
        Err(reason) => PlanApprovalSlot::Rejected {
            session_uuid: session_uuid.to_string(),
            reason,
        },
    };
    Some((plan_path, slot))
}

/// Extract and validate the `authoring-spec` fenced block from a plan.
///
/// `Ok(None)` means the plan carries no such block, i.e. it is not a Local App
/// plan. `Err` means it carries one that cannot be honoured.
fn parse_authoring_block(plan: &str) -> Result<Option<PlanAuthoringBlock>, String> {
    // (opening line, is-authoring-fence, body) for the fence currently open.
    let mut open: Option<(usize, bool, String)> = None;
    let mut bodies: Vec<String> = Vec::new();
    for (index, line) in plan.lines().enumerate() {
        if let Some(info) = line.trim_start().strip_prefix("```") {
            match open.take() {
                Some((_, true, body)) => bodies.push(body),
                Some((_, false, _)) => {}
                None => {
                    let is_authoring = info.trim().eq_ignore_ascii_case(AUTHORING_SPEC_FENCE);
                    open = Some((index + 1, is_authoring, String::new()));
                }
            }
            continue;
        }
        if let Some((_, _, body)) = open.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((start, _, _)) = open {
        return Err(format!(
            "the code fence opened at line {start} is never closed"
        ));
    }
    let body = match bodies.len() {
        0 => return Ok(None),
        1 => bodies.pop().expect("one block"),
        _ => {
            return Err(format!(
                "the plan carries more than one `{AUTHORING_SPEC_FENCE}` block; keep exactly one"
            ))
        }
    };
    let block: PlanAuthoringBlock = serde_json::from_str(body.trim()).map_err(|error| {
        format!("the `{AUTHORING_SPEC_FENCE}` block is not valid JSON: {error}")
    })?;
    if block
        .template_id
        .as_deref()
        .is_some_and(|id| id.trim().is_empty())
    {
        return Err("the authoring block's template_id must not be empty".to_string());
    }
    // Bounds are the scaffold's, checked here so a plan that cannot be
    // prepared is refused at the APPROVAL it was granted for rather than
    // after the receipt is spent. The plan was approved with these bytes, so
    // the digest binding stays meaningful either way — this only moves the
    // refusal earlier.
    for (key, value, limit) in [
        ("name", &block.name, local_apps::service::MAX_NAME_BYTES),
        ("brief", &block.brief, local_apps::service::MAX_BRIEF_BYTES),
    ] {
        if value.trim().is_empty() {
            return Err(format!("the authoring block's {key} must not be empty"));
        }
        if value.len() > limit {
            return Err(format!(
                "the authoring block's {key} is {} bytes (limit {limit})",
                value.len()
            ));
        }
    }
    block
        .spec
        .validate()
        .map_err(|error| format!("the authoring block's spec is invalid: {error}"))?;
    Ok(Some(block))
}

/// Re-verify that the plan file still holds exactly the approved revision.
///
/// The user may edit the plan in the approval dialog (the engine then persists
/// the edited text) or the model may rewrite the file afterwards. Either way a
/// changed digest means the approval no longer describes the plan on disk, and
/// the caller must send the user back through planning rather than land a
/// template for a plan nobody approved.
pub(crate) fn verify_plan_unchanged(plan_path: &str, plan_sha256: &str) -> Result<(), String> {
    let current = std::fs::read_to_string(plan_path)
        .map_err(|error| format!("cannot re-read the approved plan: {error}"))?;
    if sha256_hex(current.as_bytes()) != plan_sha256 {
        return Err(
            "the plan file changed after it was approved; ask the user to approve the edited plan"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid spec: one target, one required check, one presentation.
    fn spec_json() -> serde_json::Value {
        serde_json::json!({
            "product": {
                "goal": "Track daily water intake",
                "tasks": ["log a glass", "show today's total"],
                "external_integrations": []
            },
            "targets": [{ "id": "phone", "os": "ios", "form_factor": "phone" }],
            "ui": {
                "structure": ["header", "log button", "today's total"],
                "theme": { "mode": "system", "accent": "teal" },
                "style": { "direction": "calm", "density": "comfortable" },
                "references": []
            },
            "design": {
                "presentations": [{
                    "target_id": "phone",
                    "presentation": "single screen",
                    "navigation": "none"
                }],
                "tokens": { "radius": "16px" },
                "states": {
                    "loading": "skeleton total",
                    "empty": "prompt to log the first glass",
                    "error": "retry banner",
                    "success": "total ticks up",
                    "permission": "notifications not yet granted"
                },
                "inputs": {
                    "pointer_touch": ["tap to log"],
                    "keyboard_mouse": ["space to log"],
                    "back": "system back leaves the app",
                    "reduced_motion": "no counter animation"
                }
            },
            "acceptance_checks": [{
                "id": "log-a-glass",
                "target_ids": ["phone"],
                "required": true,
                "preconditions": [],
                "steps": ["tap the log button"],
                "expected": "today's total increases by one",
                "evidence": ["inspect"]
            }]
        })
    }

    fn plan_with_block(block: serde_json::Value) -> String {
        format!(
            "# Water tracker\n\nSome prose.\n\n```{AUTHORING_SPEC_FENCE}\n{}\n```\n",
            serde_json::to_string_pretty(&block).expect("serialize block")
        )
    }

    fn good_plan() -> String {
        plan_with_block(serde_json::json!({
            "name": "Water Tracker",
            "brief": "Log glasses of water and see today's total.",
            "template_id": "react-dom-tabs",
            "spec": spec_json(),
        }))
    }

    fn exit_result(plan: &str, plan_path: &str) -> String {
        serde_json::json!({
            "plan": plan,
            "isAgent": false,
            "filePath": plan_path,
            "hasTaskTool": true,
            "planWasEdited": false,
        })
        .to_string()
    }

    #[test]
    fn a_plan_without_the_block_is_not_a_local_app_plan() {
        let plan = "# Just a refactor\n\nNo authoring block here.\n";
        assert!(parse_authoring_block(plan).expect("no block").is_none());
        assert!(parse_plan_result(&exit_result(plan, "/tmp/p.md"), "s1").is_none());
    }

    #[test]
    fn an_ignored_fence_does_not_shadow_the_authoring_block() {
        let plan = format!("```json\n{{ \"notes\": true }}\n```\n\n{}", good_plan());
        let block = parse_authoring_block(&plan)
            .expect("block parses")
            .expect("block present");
        assert_eq!(block.template_id.as_deref(), Some("react-dom-tabs"));
    }

    #[test]
    fn a_block_without_a_name_or_brief_is_refused_at_approval() {
        for missing in ["name", "brief"] {
            let mut block = serde_json::json!({
                "name": "Water Tracker",
                "brief": "Log glasses of water.",
                "template_id": "react-dom-tabs",
                "spec": spec_json(),
            });
            block.as_object_mut().expect("object").remove(missing);
            let error = parse_authoring_block(&plan_with_block(block)).expect_err(missing);
            assert!(error.contains("not valid JSON"), "{missing}: {error}");
        }
    }

    #[test]
    fn an_empty_or_oversized_name_is_refused_at_approval() {
        let mut block = serde_json::json!({
            "name": "   ",
            "brief": "Log glasses of water.",
            "template_id": "react-dom-tabs",
            "spec": spec_json(),
        });
        let error = parse_authoring_block(&plan_with_block(block.clone())).expect_err("empty name");
        assert!(error.contains("name must not be empty"), "{error}");

        block["name"] = serde_json::json!("x".repeat(local_apps::service::MAX_NAME_BYTES + 1));
        let error = parse_authoring_block(&plan_with_block(block)).expect_err("long name");
        assert!(error.contains("limit"), "{error}");
    }

    #[test]
    fn an_approval_lands_one_app_and_stays_retryable_for_that_app() {
        let log = PlanApprovalLog::default();
        let plan = good_plan();
        let (path, slot) =
            parse_plan_result(&exit_result(&plan, "/tmp/p.md"), "s1").expect("observed");
        log.record(path.clone(), slot);

        // Another conversation cannot spend it, even knowing the plan path.
        let error = log.claim("/tmp/p.md", "s2", "app-a").expect_err("foreign");
        assert!(
            error.contains("no plan approved in THIS conversation"),
            "{error}"
        );
        // A different plan path in the right conversation is also refused.
        let error = log
            .claim("/tmp/other.md", "s1", "app-a")
            .expect_err("unknown plan");
        assert!(error.contains("no plan approved"), "{error}");

        let approval = log.claim("/tmp/p.md", "s1", "app-a").expect("first spend");
        assert_eq!(approval.name, "Water Tracker");
        // The same app retrying is not a second spend.
        log.claim("/tmp/p.md", "s1", "app-a").expect("retry");
        // A second empty shell in the same conversation is a second spend.
        let error = log
            .claim("/tmp/p.md", "s1", "app-b")
            .expect_err("second app");
        assert!(
            error.contains("already prepared a different app"),
            "{error}"
        );
    }

    #[test]
    fn a_rejected_plan_reports_its_reason_instead_of_looking_unplanned() {
        let log = PlanApprovalLog::default();
        let plan = plan_with_block(serde_json::json!({
            "name": "Water Tracker",
            "brief": "Log glasses of water.",
            "template_id": "react-dom-tabs",
            "spec": spec_json(),
            "unexpected": 1,
        }));
        let (path, slot) =
            parse_plan_result(&exit_result(&plan, "/tmp/p.md"), "s1").expect("observed");
        log.record(path.clone(), slot);
        let error = log.claim("/tmp/p.md", "s1", "app-a").expect_err("rejected");
        assert!(error.contains("plan_approval_invalid"), "{error}");
        assert!(error.contains("not valid JSON"), "{error}");
    }

    #[test]
    fn a_subagent_plan_is_never_a_user_approval() {
        let plan = good_plan();
        let result = serde_json::json!({
            "plan": plan,
            "isAgent": true,
            "filePath": "/tmp/agent.md",
        })
        .to_string();
        assert!(parse_plan_result(&result, "s1").is_none());
    }

    #[test]
    fn a_successful_exit_records_the_digest_and_template() {
        let plan = good_plan();
        let result = exit_result(&plan, "/tmp/plan.md");
        let (path, slot) = parse_plan_result(&result, "s1").expect("observable");
        assert_eq!(path, "/tmp/plan.md");
        let PlanApprovalSlot::Approved(approval) = slot else {
            panic!("expected an approval");
        };
        assert_eq!(approval.template_id.as_deref(), Some("react-dom-tabs"));
        assert_eq!(
            approval.plan_sha256,
            local_apps::sha256_hex(plan.as_bytes())
        );
        assert_eq!(approval.session_uuid, "s1");
    }

    #[test]
    fn a_malformed_block_is_recorded_as_a_rejection_not_an_approval() {
        let plan = plan_with_block(serde_json::json!({
            "name": "Water Tracker",
            "brief": "Log glasses of water.",
            "template_id": "react-dom-tabs",
            "spec": spec_json(),
            "unexpected": 1,
        }));
        let (_, slot) =
            parse_plan_result(&exit_result(&plan, "/tmp/p.md"), "s1").expect("observed");
        let PlanApprovalSlot::Rejected { reason, .. } = slot else {
            panic!("a closed block may not be honoured");
        };
        assert!(
            reason.contains("not valid JSON"),
            "unexpected reason: {reason}"
        );
    }

    #[test]
    fn an_unterminated_block_fails_closed() {
        let plan = format!("```{AUTHORING_SPEC_FENCE}\n{{}}\n");
        let error = parse_authoring_block(&plan).expect_err("unterminated");
        assert!(error.contains("never closed"), "unexpected error: {error}");
    }

    #[test]
    fn a_spec_that_fails_validation_fails_closed() {
        let mut spec = spec_json();
        spec["acceptance_checks"] = serde_json::json!([]);
        let plan = plan_with_block(serde_json::json!({
            "name": "Water Tracker",
            "brief": "Log glasses of water.",
            "template_id": "react-dom-tabs",
            "spec": spec,
        }));
        let error = parse_authoring_block(&plan).expect_err("invalid spec");
        assert!(
            error.contains("acceptance_checks"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_approved_plan_is_not_transferable_to_another_conversation() {
        let log = PlanApprovalLog::default();
        let plan = good_plan();
        let (path, slot) = parse_plan_result(&exit_result(&plan, "/tmp/p.md"), "s1").expect("ok");
        log.record(path.clone(), slot);
        assert!(log.claim("/tmp/p.md", "s1", "app-1").is_ok());
        assert!(log.claim("/tmp/p.md", "s2", "app-1").is_err());
        assert!(log.claim("/tmp/other.md", "s1", "app-1").is_err());
    }

    #[test]
    fn an_edited_plan_no_longer_matches_the_approval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plan.md");
        let plan = good_plan();
        std::fs::write(&path, &plan).expect("write plan");
        let (_, slot) =
            parse_plan_result(&exit_result(&plan, &path.to_string_lossy()), "s1").expect("ok");
        let PlanApprovalSlot::Approved(approval) = slot else {
            panic!("expected an approval");
        };
        verify_plan_unchanged(&approval.plan_path, &approval.plan_sha256)
            .expect("unchanged plan verifies");
        std::fs::write(&path, format!("{plan}\n- a late extra step\n")).expect("edit plan");
        let error = verify_plan_unchanged(&approval.plan_path, &approval.plan_sha256)
            .expect_err("edited plan is refused");
        assert!(error.contains("changed after it was approved"), "{error}");
    }

    #[tokio::test]
    async fn the_watcher_forwards_every_event_and_records_the_approval() {
        #[derive(Default)]
        struct Recorder {
            seen: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl ClientEventListener for Recorder {
            async fn on_event(&self, event: ClientEvent) {
                let label = match event {
                    ClientEvent::TextDelta { text } => text,
                    ClientEvent::ToolUseResult { tool, .. } => tool,
                    other => format!("{other:?}"),
                };
                self.seen.lock().expect("lock").push(label);
            }
        }

        let recorder = Arc::new(Recorder::default());
        let log = Arc::new(PlanApprovalLog::default());
        let session = Arc::new(Mutex::new("s1".to_string()));
        let watcher = PlanApprovalWatcher::new(recorder.clone(), log.clone(), session);

        let plan = good_plan();
        watcher
            .on_event(ClientEvent::ToolUseResult {
                id: "t1".to_string(),
                tool: "ExitPlanMode".to_string(),
                result_json: exit_result(&plan, "/tmp/p.md"),
                is_error: false,
                display: None,
            })
            .await;
        // A denied exit carries an error payload and must not be recorded.
        watcher
            .on_event(ClientEvent::ToolUseResult {
                id: "t2".to_string(),
                tool: "ExitPlanMode".to_string(),
                result_json: serde_json::json!({ "error": "denied" }).to_string(),
                is_error: true,
                display: None,
            })
            .await;
        watcher
            .on_event(ClientEvent::TextDelta {
                text: "hello".to_string(),
            })
            .await;

        // Transparent: every event still reached the wrapped listener, in order.
        assert_eq!(
            *recorder.seen.lock().expect("lock"),
            vec![
                "ExitPlanMode".to_string(),
                "ExitPlanMode".to_string(),
                "hello".to_string(),
            ]
        );
        let recorded = log
            .claim("/tmp/p.md", "s1", "app-1")
            .expect("the approved plan claims for its own conversation");
        assert_eq!(recorded.template_id.as_deref(), Some("react-dom-tabs"));
    }
}
