//! Invocation-local classifier-only report review composition.

use crate::classifier::AutoModeClassifierVerdict;
use crate::loop_llm::{ClassifierFailure, DetailedClassification};
use lingxi_core::host::handback::ReportReview;
use lingxi_core::host::permission_gate::{
    ClassifierOnlyOnBlock, ClassifierOnlyOutcome, ClassifierOnlyPolicy, PermissionOutcome,
};

/// Preserve `JEt`'s failure distinctions before ordinary permissions lower them.
pub fn report_review(result: DetailedClassification) -> Option<ReportReview> {
    match result.failure {
        Some(ClassifierFailure::Refused) => Some(ReportReview::Refused),
        Some(ClassifierFailure::Unavailable {
            http_status,
            error_kind,
        }) => Some(ReportReview::Unavailable {
            model: result.model,
            http_status,
            error_kind,
            failure_kind: None,
        }),
        Some(ClassifierFailure::TranscriptTooLong) => Some(ReportReview::Unavailable {
            model: result.model,
            http_status: None,
            error_kind: None,
            failure_kind: Some("transcript_too_long".into()),
        }),
        Some(ClassifierFailure::Unparseable { .. }) => Some(ReportReview::Unavailable {
            model: result.model,
            http_status: None,
            error_kind: None,
            failure_kind: Some("unparseable".into()),
        }),
        None => match result.verdict {
            AutoModeClassifierVerdict::Allow { .. } => Some(ReportReview::Passed),
            AutoModeClassifierVerdict::Deny { reason, .. } => {
                Some(ReportReview::Blocked { reason })
            }
            // These contain no typed proof that this binding ran a classifier.
            AutoModeClassifierVerdict::Pass { .. }
            | AutoModeClassifierVerdict::NoVerdict { .. }
            | AutoModeClassifierVerdict::TranscriptTooLong => None,
        },
    }
}

/// Never prompt, and never manufacture review from a permission allow.
pub fn permission_outcome(
    name: &str,
    policy: ClassifierOnlyPolicy,
    review: Option<ReportReview>,
) -> ClassifierOnlyOutcome {
    let permission = match review.as_ref() {
        Some(ReportReview::Passed) => allowed(),
        Some(_) if policy.on_block == ClassifierOnlyOnBlock::Flag => allowed(),
        Some(ReportReview::Blocked { reason }) => PermissionOutcome::Deny {
            reason: reason.clone(),
        },
        Some(ReportReview::Refused | ReportReview::Unavailable { .. }) | None => {
            PermissionOutcome::Deny {
                reason: format!(
                    "Only the auto-mode classifier can allow {name}: no prompt is raised for it, and the classifier did not allow it"
                ),
            }
        }
    };
    ClassifierOnlyOutcome { permission, review }
}

fn allowed() -> PermissionOutcome {
    PermissionOutcome::Allow {
        updated_input: None,
        permission_updates: Vec::new(),
        decision_classification: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rendered_parse_failure_is_still_an_unavailable_report_review() {
        let parsed = report_review(DetailedClassification {
            model: "test-model".into(),
            verdict: AutoModeClassifierVerdict::Deny {
                score: 1.0,
                reason: crate::loop_llm::UNAVAILABLE.into(),
                hard: false,
            },
            failure: Some(ClassifierFailure::Unparseable {
                stage: "stage 2",
                stop_reason: "max_tokens".into(),
            }),
        });
        assert!(matches!(
            parsed,
            Some(ReportReview::Unavailable { failure_kind: Some(kind), .. })
                if kind == "unparseable"
        ));
    }

    #[test]
    fn matching_reason_text_cannot_invent_an_unavailable_review() {
        let reviewed = report_review(DetailedClassification {
            model: "test-model".into(),
            verdict: AutoModeClassifierVerdict::Deny {
                score: 1.0,
                reason: crate::loop_llm::UNAVAILABLE.into(),
                hard: false,
            },
            failure: None,
        });
        assert!(matches!(reviewed, Some(ReportReview::Blocked { .. })));
    }

    #[test]
    fn no_verdict_is_rejected_even_by_a_flagging_policy() {
        let result = permission_outcome(
            "SubagentHandback",
            ClassifierOnlyPolicy {
                on_block: ClassifierOnlyOnBlock::Flag,
            },
            None,
        );
        assert!(matches!(result.permission, PermissionOutcome::Deny { .. }));
        assert_eq!(result.review, None);
    }
}
