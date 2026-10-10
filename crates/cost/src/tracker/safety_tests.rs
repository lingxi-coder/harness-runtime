use super::*;
use lingxi_core::host::model_safety::ModelSafetyStop;

fn tracker(session: SessionId) -> CostTracker {
    let (sender, _receiver) = mpsc::channel(8);
    CostTracker::new(session, Arc::new(PricingCatalog::empty()), sender)
}

#[tokio::test]
async fn safety_events_are_ephemeral_and_fresh_hydrated_owner_starts_at_zero() {
    let session = SessionId::new();
    let original = tracker(session);
    original.selected_entry().state.write().await.total_nano_usd = 42;
    let observer = original.session_scope(session).model_safety_observer();
    observer.record(ModelSafetyStop::Refusal);
    observer.record(ModelSafetyStop::ContentFiltering);
    assert_eq!(original.safety_stops(), 2);
    let persisted = serde_json::to_value(original.snapshot().await).unwrap();
    assert!(persisted.get("safety_stops").is_none());
    let resumed = tracker(session);
    *resumed.selected_entry().state.write().await = serde_json::from_value(persisted).unwrap();
    assert_eq!(resumed.snapshot().await.total_nano_usd, 42);
    assert_eq!(resumed.safety_stops(), 0);
    observer.record(ModelSafetyStop::Refusal);
    assert_eq!(original.safety_stops(), 3);
    assert_eq!(
        resumed.safety_stops(),
        0,
        "old authority's late observation cannot target a fresh mount"
    );
}

#[tokio::test]
async fn clear_resets_the_current_authority_in_place_without_retargeting_other_sessions() {
    let session = SessionId::new();
    let owner = tracker(session);
    let observer = owner.session_scope(session).model_safety_observer();
    observer.record(ModelSafetyStop::Refusal);
    owner.reset().await;
    assert_eq!(owner.safety_stops(), 0);
    observer.record(ModelSafetyStop::ContentFiltering);
    assert_eq!(
        owner.safety_stops(),
        1,
        "the same authority can receive a late event after reset"
    );
    let other = owner.scoped(SessionId::new());
    observer.record(ModelSafetyStop::Refusal);
    assert_eq!(other.safety_stops(), 0);
    assert_eq!(owner.safety_stops(), 2);
}
