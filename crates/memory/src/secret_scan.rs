//! Thin adapter over `secret::SecretScanner`.
//!
//! Reuses the v3 §16.5 30+ gitleaks rules — we do NOT duplicate the rule
//! set here. The scanner is built once via [`SecretScanner::builtin`] and
//! shared across loader passes.

use secret::{SecretDetection, SecretScanner};
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// Telemetry event name emitted on each per-rule redaction hit.
pub const TENGU_MEMORY_SECRET_REDACTED: &str = "tengu_memory_secret_redacted";

fn scanner() -> &'static SecretScanner {
    static SCANNER: OnceLock<SecretScanner> = OnceLock::new();
    SCANNER.get_or_init(SecretScanner::builtin)
}

/// Redact secrets in `content`, returning the redacted text and any
/// detections so callers can emit `tengu_memory_secret_redacted` events
/// with PII discipline (path is PII; rule id is not).
#[must_use]
pub fn redact_with_detections(content: &str) -> (String, Vec<SecretDetection>) {
    let s = scanner();
    let dets = s.scan(content);
    let red = s.redact(content);
    (red, dets)
}

/// Redact-only variant (no detections returned). Convenience for
/// callers that don't need telemetry.
#[must_use]
pub fn redact(content: &str) -> String {
    scanner().redact(content)
}

/// Emit `tengu_memory_secret_redacted` once per detection.
///
/// Payload (locked): `rule_id: Verified(<rule id>)`, `_PROTO_path: PiiTagged(<path>)`.
/// The matched secret value itself is NEVER emitted.
pub async fn emit_redactions(
    bus: Option<&Arc<telemetry::AnalyticsBus>>,
    path: &Path,
    detections: &[SecretDetection],
) {
    let Some(bus) = bus else {
        return;
    };
    for det in detections {
        let mut md = telemetry::sink::LogEventMetadata::new();
        md.insert(
            "rule_id".into(),
            telemetry::sink::AnalyticsValue::String(
                telemetry::pii::Verified::assert_safe(det.rule_id.clone()).into_inner(),
            ),
        );
        md.insert(
            "_PROTO_path".into(),
            telemetry::sink::AnalyticsValue::String(
                telemetry::pii::PiiTagged::assert_pii_tagged_column(path.display().to_string())
                    .into_inner(),
            ),
        );
        bus.log_event(TENGU_MEMORY_SECRET_REDACTED, md).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use telemetry::{sink::LogEventMetadata, AnalyticsBus, AnalyticsSink, AnalyticsValue};

    #[test]
    fn no_secrets_passes_through_unchanged() {
        let (out, dets) = redact_with_detections("just some markdown\nwith no keys\n");
        assert_eq!(out, "just some markdown\nwith no keys\n");
        assert!(dets.is_empty());
    }

    #[test]
    fn aws_access_token_is_redacted() {
        let raw = "creds: AKIAIOSFODNN7EXAMPLE";
        let (out, dets) = redact_with_detections(raw);
        assert!(
            out.contains("[REDACTED:"),
            "must include REDACTED marker, got {out:?}"
        );
        assert!(
            dets.iter().any(|d| d.rule_id == "aws-access-token"),
            "aws rule must fire, got {dets:?}"
        );
    }

    #[test]
    fn redact_alias_returns_same_string_as_with_detections() {
        let raw = "creds: AKIAIOSFODNN7EXAMPLE";
        let only = redact(raw);
        let (with_dets, _) = redact_with_detections(raw);
        assert_eq!(only, with_dets);
    }

    struct Cap {
        events: Mutex<Vec<(String, LogEventMetadata)>>,
    }
    #[async_trait::async_trait]
    impl AnalyticsSink for Cap {
        async fn log_event(&self, n: &str, m: LogEventMetadata) {
            self.events.lock().unwrap().push((n.into(), m));
        }
        async fn log_event_async(&self, n: &str, m: LogEventMetadata) {
            self.log_event(n, m).await;
        }
        fn name(&self) -> &str {
            "cap"
        }
    }

    #[tokio::test]
    async fn emit_redactions_writes_one_event_per_detection() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(Cap {
            events: Mutex::new(Vec::new()),
        });
        bus.attach_sink(sink.clone()).await;
        let dets = vec![
            SecretDetection {
                rule_id: "aws-access-token".into(),
                label: "AWS".into(),
            },
            SecretDetection {
                rule_id: "stripe".into(),
                label: "Stripe".into(),
            },
        ];
        emit_redactions(Some(&bus), Path::new("/x/LINGXI.md"), &dets).await;
        let ev = sink.events.lock().unwrap();
        assert_eq!(ev.len(), 2);
        for (name, md) in ev.iter() {
            assert_eq!(name, "tengu_memory_secret_redacted");
            assert!(md.contains_key("_PROTO_path"));
            assert!(md.contains_key("rule_id"));
            matches!(md.get("rule_id"), Some(AnalyticsValue::String(_)));
        }
    }
}
