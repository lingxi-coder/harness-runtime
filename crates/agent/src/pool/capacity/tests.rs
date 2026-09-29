use super::*;
use futures::poll;

#[tokio::test]
async fn zero_capacity_disables_allocations_without_constructor_failure() {
    let core = CapacityCore::new(0).unwrap();
    assert!(matches!(core.acquire_ordinary(), Err(AdmissionError::Full)));
    for count in [0, 1] {
        assert!(matches!(
            core.reserve_group(count, deadline(), CancellationToken::new())
                .await,
            Err(AdmissionError::InvalidCount)
        ));
    }
    if Semaphore::MAX_PERMITS > u32::MAX as usize {
        let count = (u32::MAX as usize).checked_add(1).unwrap();
        let large = CapacityCore::new(count).unwrap();
        assert!(large.acquire_ordinary().is_ok());
        assert!(matches!(
            large
                .reserve_group(count, deadline(), CancellationToken::new())
                .await,
            Err(AdmissionError::InvalidCount)
        ));
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(100)
}

#[tokio::test]
async fn producer_ticket_waits_for_every_physical_owner_without_holding_capacity() {
    use lingxi_core::host::panel_pool::PanelPoolDrain;
    let core = CapacityCore::new(2).unwrap();
    let mut permits = core
        .reserve_group(2, deadline(), CancellationToken::new())
        .await
        .unwrap();
    let ticket = PoolGroupDrain::new(2);
    for permit in &mut permits {
        permit.track_group(ticket.clone());
    }
    let first = Arc::new(permits.pop().unwrap());
    let runner = first.clone();
    let second = permits.pop().unwrap();
    let mut waiting = Box::pin(ticket.wait());
    assert!(poll!(&mut waiting).is_pending());
    drop(first);
    drop(second);
    assert_eq!(core.available_permits(), 1);
    assert!(poll!(&mut waiting).is_pending());
    drop(runner);
    waiting.await;
    assert_eq!(core.available_permits(), 2);
    // Waiting repeatedly neither consumes nor owns physical capacity.
    ticket.wait().await;
    assert!(core.acquire_ordinary().is_ok());
}

#[tokio::test]
async fn a_waiting_group_holds_its_share_and_dropping_it_unblocks_the_tail() {
    let core = CapacityCore::new(3).unwrap();
    let held = core.acquire_ordinary().unwrap();
    let mut head = Box::pin(core.reserve_group(3, deadline(), CancellationToken::new()));
    let mut tail = Box::pin(core.reserve_group(1, deadline(), CancellationToken::new()));
    assert!(poll!(&mut head).is_pending());
    // FIFO admission assigns freed permits to the waiter at the front, so a
    // blocked group holds what it has already been granted rather than
    // repeatedly re-testing for the whole set. That is what stops a steady
    // stream of small groups from starving a large one.
    assert_eq!(core.available_permits(), 0);
    assert!(poll!(&mut tail).is_pending());
    // Nothing partial ever escaped: the head never yielded a permit.
    drop(head);
    let group = tail.await.unwrap();
    assert_eq!(group.len(), 1);
    assert_eq!(core.available_permits(), 1);
    drop(group);
    drop(held);
    assert_eq!(core.available_permits(), 3);
}

#[tokio::test(start_paused = true)]
async fn fixed_deadline_does_not_reset_on_release_notifications() {
    let core = CapacityCore::new(2).unwrap();
    let held = core.acquire_ordinary().unwrap();
    let mut group = Box::pin(core.reserve_group(2, deadline(), CancellationToken::new()));
    assert!(poll!(&mut group).is_pending());
    tokio::time::advance(Duration::from_secs(20)).await;
    // A release that still leaves the group unsatisfiable must not extend the
    // deadline: it is fixed at the first poll, not re-armed per wakeup.
    let spare = core.acquire_ordinary();
    assert!(spare.is_err(), "capacity is occupied");
    assert!(poll!(&mut group).is_pending());
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(matches!(group.await, Err(AdmissionError::Deadline)));
    let mut short = Box::pin(core.reserve_group(
        2,
        Instant::now() + Duration::from_secs(2),
        CancellationToken::new(),
    ));
    assert!(poll!(&mut short).is_pending());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(short.await, Err(AdmissionError::Deadline)));
    drop(held);
}

#[tokio::test]
async fn invalid_count_identity_and_unpolled_drop() {
    let core = CapacityCore::new(2).unwrap();
    let other = CapacityCore::new(2).unwrap();
    assert_eq!(core.max(), 2);
    for count in [0, 3] {
        assert!(matches!(
            core.reserve_group(count, deadline(), CancellationToken::new())
                .await,
            Err(AdmissionError::InvalidCount)
        ));
    }
    let permit = core.acquire_ordinary().unwrap();
    assert!(core.owns(&permit));
    assert!(!other.owns(&permit));
    // An unpolled reservation future holds nothing.
    drop(core.reserve_group(2, deadline(), CancellationToken::new()));
    assert_eq!(core.available_permits(), 1);
    drop(permit);
    assert_eq!(core.available_permits(), 2);
    assert_eq!(other.available_permits(), 2);
}
