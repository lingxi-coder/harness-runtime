use super::*;

#[test]
fn playback_ack_commits_once_and_interrupt_discards_unheard_transcripts() {
    let mut ledger = TranscriptLedger::default();
    let limits = RealtimeAgentLimits::default();
    ledger.generated.insert(Some("heard".into()));
    assert!(ledger
        .output_final(Some("heard".into()), "heard text".into(), limits)
        .unwrap()
        .is_none());
    assert_eq!(
        ledger.playback(Some("heard".into())).as_deref(),
        Some("heard text")
    );
    assert!(ledger
        .output_final(Some("heard".into()), "duplicate".into(), limits)
        .unwrap()
        .is_none());
    ledger.generated.insert(Some("unheard".into()));
    ledger.interrupt();
    assert!(ledger
        .output_final(Some("unheard".into()), "not heard".into(), limits)
        .unwrap()
        .is_none());
    assert!(ledger.playback(Some("unheard".into())).is_none());
    ledger.generated.insert(Some("ack-first".into()));
    assert!(ledger.playback(Some("ack-first".into())).is_none());
    assert_eq!(
        ledger
            .output_final(Some("ack-first".into()), "heard early".into(), limits)
            .unwrap()
            .as_deref(),
        Some("heard early")
    );
}
