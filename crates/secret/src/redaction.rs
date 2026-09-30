//! Boundary-aware redaction policy.
//!
//! Different egress boundaries (logs, telemetry, team-memory upload, ...)
//! have different acceptable-risk profiles. [`RedactionPolicy`] composes a
//! [`SecretScanner`] with a per-boundary [`BoundaryPolicy`] map and returns a
//! [`RedactionOutcome`] describing the action taken.

use crate::scanner::{SecretDetection, SecretScanner};
use lingxi_core::types::RedactableContent;
use std::collections::HashMap;
use std::sync::Arc;

/// Egress boundary at which redaction is being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RedactionBoundary {
    /// Persisting to the on-disk transcript / replay log.
    TranscriptWrite,
    /// Emitting to stdout/stderr or a structured log sink.
    LogOutput,
    /// Uploading to a shared team memory store.
    TeamMemoryUpload,
    /// Injecting content back into the active conversation.
    ConversationInjection,
    /// Emitting to a telemetry / analytics backend.
    Telemetry,
}

/// Action a [`RedactionPolicy`] takes when its scanner fires at a boundary.
#[derive(Debug, Clone, Copy)]
pub enum BoundaryPolicy {
    /// Always rewrite the content (replace matches with `[REDACTED:<id>]`).
    AlwaysRedact,
    /// Refuse to forward the content at all.
    RejectOnDetection,
    /// Forward the content unchanged and surface a warning via detections.
    WarnOnly,
}

/// Policy that decides what to do with potentially-secret content per
/// boundary.
pub struct RedactionPolicy {
    scanner: Arc<SecretScanner>,
    boundary: HashMap<RedactionBoundary, BoundaryPolicy>,
}

/// Outcome returned by [`RedactionPolicy::process`].
#[derive(Debug, Clone)]
pub enum RedactionOutcome {
    /// No matches; original passes through.
    Clean {
        /// The original content, unchanged.
        content: RedactableContent,
    },
    /// Match(es) found; rewritten content + list of detections.
    Redacted {
        /// Possibly-rewritten content, depending on the firing boundary policy.
        content: RedactableContent,
        /// List of every rule that fired.
        detections: Vec<SecretDetection>,
    },
    /// Boundary policy rejects egress; content discarded.
    Rejected {
        /// List of every rule that fired before rejection.
        detections: Vec<SecretDetection>,
    },
}

impl RedactionPolicy {
    /// Build a policy with the default per-boundary configuration:
    /// `Telemetry`, `TranscriptWrite`, `LogOutput`, and
    /// `ConversationInjection` are `AlwaysRedact`; `TeamMemoryUpload` is
    /// `RejectOnDetection`.
    #[must_use]
    pub fn with_defaults(scanner: Arc<SecretScanner>) -> Self {
        let mut map = HashMap::new();
        map.insert(
            RedactionBoundary::TranscriptWrite,
            BoundaryPolicy::AlwaysRedact,
        );
        map.insert(RedactionBoundary::LogOutput, BoundaryPolicy::AlwaysRedact);
        map.insert(
            RedactionBoundary::TeamMemoryUpload,
            BoundaryPolicy::RejectOnDetection,
        );
        map.insert(
            RedactionBoundary::ConversationInjection,
            BoundaryPolicy::AlwaysRedact,
        );
        map.insert(RedactionBoundary::Telemetry, BoundaryPolicy::AlwaysRedact);
        Self {
            scanner,
            boundary: map,
        }
    }

    /// Evaluate `content` against the configured `boundary` policy and return
    /// the resulting [`RedactionOutcome`].
    ///
    /// Boundaries without an explicit entry default to
    /// [`BoundaryPolicy::AlwaysRedact`].
    #[must_use]
    pub fn process(
        &self,
        content: RedactableContent,
        boundary: RedactionBoundary,
    ) -> RedactionOutcome {
        let raw = content.expose_for_scan();
        let detections = self.scanner.scan(raw);
        if detections.is_empty() {
            return RedactionOutcome::Clean { content };
        }
        match self
            .boundary
            .get(&boundary)
            .copied()
            .unwrap_or(BoundaryPolicy::AlwaysRedact)
        {
            BoundaryPolicy::AlwaysRedact => {
                let rewritten = self.scanner.redact(raw);
                RedactionOutcome::Redacted {
                    content: RedactableContent::new(rewritten),
                    detections,
                }
            }
            BoundaryPolicy::RejectOnDetection => RedactionOutcome::Rejected { detections },
            BoundaryPolicy::WarnOnly => RedactionOutcome::Redacted {
                content,
                detections,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_redacts_aws_token() {
        let scanner = Arc::new(SecretScanner::builtin());
        let policy = RedactionPolicy::with_defaults(scanner);
        let c = RedactableContent::new("token AKIAIOSFODNN7EXAMPLE here".into());
        let out = policy.process(c, RedactionBoundary::Telemetry);
        match out {
            RedactionOutcome::Redacted {
                content,
                detections,
            } => {
                assert!(!content.expose_for_scan().contains("AKIA"));
                assert!(!detections.is_empty());
            }
            other => panic!("expected Redacted, got {other:?}"),
        }
    }

    #[test]
    fn team_memory_rejects_on_detection() {
        let scanner = Arc::new(SecretScanner::builtin());
        let policy = RedactionPolicy::with_defaults(scanner);
        let c = RedactableContent::new("ghp_1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ".into());
        let out = policy.process(c, RedactionBoundary::TeamMemoryUpload);
        assert!(matches!(out, RedactionOutcome::Rejected { .. }));
    }
}
