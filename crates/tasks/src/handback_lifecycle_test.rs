use super::*;

struct AdmissionGate {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

/// The root owns its registry, as the shipping orchestrator does. A strong
/// admission binding in the registry would close a root -> registry -> root
/// cycle, even after the host releases its last root handle.
struct RootAdmission {
    registry: Arc<TaskRegistry>,
    scope: HandbackSessionScope,
    gate: Option<AdmissionGate>,
    accepted: Arc<StdMutex<Vec<HandbackEnvelope>>>,
}

#[async_trait]
impl ReportingAdmission for RootAdmission {
    async fn main_scope(&self) -> Option<HandbackSessionScope> {
        Some(self.scope)
    }

    async fn admit(&self, envelope: HandbackEnvelope) -> Result<(), HandbackAdmissionError> {
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        if envelope.receipt.recipient != (HandbackRecipient::Main { scope: self.scope }) {
            return Err(HandbackAdmissionError::StaleScope);
        }
        self.accepted.lock().unwrap().push(envelope);
        Ok(())
    }
}

#[tokio::test]
async fn root_release_retires_admission_without_retiring_a_live_handback_token() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let scope = scope(1);
    let accepted = Arc::new(StdMutex::new(Vec::new()));
    let root = Arc::new(RootAdmission {
        registry: registry.clone(),
        scope,
        gate: None,
        accepted: accepted.clone(),
    });
    assert!(Arc::ptr_eq(&root.registry, &registry));
    let weak_root = Arc::downgrade(&root);
    assert!(registry.bind_reporting_admission(Arc::downgrade(
        &(root.clone() as Arc<dyn ReportingAdmission>),
    )));
    let actor = AgentId::new();
    actor_row(&registry, "root-lifecycle-child", actor, false, None).await;
    let token = begin(&registry, actor, scope, None).await;

    drop(root);
    assert!(
        weak_root.upgrade().is_none(),
        "the registry must not keep its root alive through the inverse admission edge"
    );
    assert_eq!(registry.handback_scope().await, None);
    assert_eq!(
        registry.try_deliver_handback(&token, report()).await,
        HandbackAdmissionOutcome::Rejected,
        "a live run cannot obtain a receipt after its root admission authority is gone"
    );
    let state = registry
        .handback_state(&token)
        .await
        .expect("root release does not invalidate the existing run token");
    assert_eq!(state.run, token.run());
    assert!(state.active);
    assert!(state.receipt.is_none());
    assert!(state.report.is_none());
    assert!(state.disposition.is_none());
    assert!(accepted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn admission_transaction_holds_its_upgraded_root_until_the_receipt_is_committed() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let scope = scope(1);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let accepted = Arc::new(StdMutex::new(Vec::new()));
    let root = Arc::new(RootAdmission {
        registry: registry.clone(),
        scope,
        gate: Some(AdmissionGate {
            entered: entered.clone(),
            release: release.clone(),
        }),
        accepted: accepted.clone(),
    });
    assert!(Arc::ptr_eq(&root.registry, &registry));
    let weak_root = Arc::downgrade(&root);
    assert!(registry.bind_reporting_admission(Arc::downgrade(
        &(root.clone() as Arc<dyn ReportingAdmission>),
    )));
    let actor = AgentId::new();
    actor_row(&registry, "gated-root-lifecycle-child", actor, false, None).await;
    let token = begin(&registry, actor, scope, None).await;
    let prepared = report();
    let mut delivery = {
        let registry = registry.clone();
        let token = token.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move { registry.try_deliver_handback(&token, prepared).await })
    };
    tokio::select! {
        _ = entered.notified() => {}
        result = &mut delivery => {
            panic!("delivery must reach the real gated admission before settling: {result:?}");
        }
    }

    drop(root);
    assert!(
        weak_root.upgrade().is_some(),
        "the registry's temporary upgrade must span the pending admit await"
    );
    assert!(accepted.lock().unwrap().is_empty());
    // This handle belongs to the test rather than the root, so releasing the
    // gate cannot accidentally add another strong root owner.
    release.notify_one();
    let HandbackAdmissionOutcome::Admitted(receipt) = delivery.await.unwrap() else {
        panic!("an admission already in progress must commit its receipt");
    };
    assert!(
        weak_root.upgrade().is_none(),
        "the root must drop as soon as the admission transaction settles"
    );
    assert_eq!(receipt.message_id, prepared.message_id);
    let state = registry.handback_state(&token).await.unwrap();
    assert_eq!(state.receipt, Some(receipt.clone()));
    assert_eq!(state.report, Some(prepared.report));
    assert_eq!(state.disposition, Some(HandbackDisposition::Send));
    {
        let accepted = accepted.lock().unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].receipt, receipt);
    }
    assert_eq!(registry.handback_scope().await, None);
}
