use super::*;
use crate::thread_state::ThreadTransitionReservationPhase;
use pretty_assertions::assert_eq;

#[test]
fn unloading_target_conversion_uses_saturating_remaining_duration() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_secs();
    let target = Instant::now() + Duration::from_secs(60);

    let converted = unloading_target_unix(target);

    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_secs();
    assert!(converted >= i64::try_from(before.saturating_add(59)).unwrap_or(i64::MAX));
    assert!(converted <= i64::try_from(after.saturating_add(60)).unwrap_or(i64::MAX));
}

#[tokio::test]
async fn auto_subscribe_preserves_live_transition_reservation() -> anyhow::Result<()> {
    let manager = ThreadStateManager::new();
    let pending_thread_unloads = Mutex::new(HashSet::new());
    let thread_id = ThreadId::new();
    let current_thread_id = ThreadId::new();
    let initiator = ConnectionId(1);
    let newcomer = ConnectionId(2);
    for connection_id in [initiator, newcomer] {
        manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
    }
    manager
        .try_ensure_connection_subscribed(
            thread_id, initiator, /*experimental_raw_events*/ false,
        )
        .await
        .expect("initiator should subscribe");
    manager
        .reserve_thread_transition(thread_id, initiator, "transition-1".to_string())
        .await
        .expect("transition should reserve the thread");
    manager
        .mark_thread_transition_prepared(thread_id, "transition-1", current_thread_id)
        .await
        .expect("transition should become prepared");
    assert!(
        manager
            .unsubscribe_connection_from_thread(thread_id, initiator)
            .await
    );

    let subscription = manager
        .try_ensure_connection_subscribed_unless_pending_unload(
            &pending_thread_unloads,
            thread_id,
            newcomer,
            /*experimental_raw_events*/ false,
        )
        .await;
    assert!(matches!(
        subscription,
        ConversationSubscription::TransitionInProgress
    ));
    let reservation = manager
        .thread_transition_reservation(thread_id)
        .await
        .expect("transition reservation should remain");
    assert_eq!(
        (
            reservation.transition_id,
            reservation.initiator_connection_id,
            reservation.current_thread_id,
            reservation.phase,
            reservation.invalid_reason,
        ),
        (
            "transition-1".to_string(),
            initiator,
            Some(current_thread_id),
            ThreadTransitionReservationPhase::InitiatorUnsubscribed,
            None,
        )
    );
    assert_eq!(
        manager.caller_subscription(thread_id, newcomer).await,
        (false, 0)
    );
    assert!(
        manager
            .acquire_thread_mutation_permit(thread_id)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn repeated_auto_subscribe_reports_unchanged_and_preserves_raw_event_opt_in()
-> anyhow::Result<()> {
    let manager = ThreadStateManager::new();
    let pending_thread_unloads = Mutex::new(HashSet::new());
    let thread_id = ThreadId::new();
    let connection_id = ConnectionId(1);
    manager
        .connection_initialized(connection_id, ConnectionCapabilities::default())
        .await;

    let first = manager
        .try_ensure_connection_subscribed_unless_pending_unload(
            &pending_thread_unloads,
            thread_id,
            connection_id,
            /*experimental_raw_events*/ false,
        )
        .await;
    assert!(matches!(
        first,
        ConversationSubscription::Subscribed {
            subscription_changed: true,
            ..
        }
    ));
    let second = manager
        .try_ensure_connection_subscribed_unless_pending_unload(
            &pending_thread_unloads,
            thread_id,
            connection_id,
            /*experimental_raw_events*/ true,
        )
        .await;
    let ConversationSubscription::Subscribed {
        thread_state,
        subscription_changed,
        ..
    } = second
    else {
        panic!("existing live connection should remain subscribed");
    };
    assert!(!subscription_changed);
    assert!(thread_state.lock().await.experimental_raw_events);

    manager.remove_connection(connection_id).await;
    assert!(matches!(
        manager
            .try_ensure_connection_subscribed_unless_pending_unload(
                &pending_thread_unloads,
                thread_id,
                connection_id,
                /*experimental_raw_events*/ false,
            )
            .await,
        ConversationSubscription::ConnectionClosed
    ));
    Ok(())
}

#[tokio::test]
async fn running_resume_subscription_reports_transition_and_preserves_initiator_abort()
-> anyhow::Result<()> {
    let manager = ThreadStateManager::new();
    let pending_thread_unloads = Mutex::new(HashSet::new());
    let thread_id = ThreadId::new();
    let current_thread_id = ThreadId::new();
    let initiator = ConnectionId(1);
    let newcomer = ConnectionId(2);
    for connection_id in [initiator, newcomer] {
        manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
    }
    manager
        .try_ensure_connection_subscribed(
            thread_id, initiator, /*experimental_raw_events*/ false,
        )
        .await
        .expect("initiator should subscribe");
    manager
        .reserve_thread_transition(thread_id, initiator, "transition-1".to_string())
        .await
        .expect("transition should reserve the thread");

    assert!(matches!(
        manager
            .try_add_connection_to_thread_unless_pending_unload(
                &pending_thread_unloads,
                thread_id,
                newcomer,
            )
            .await,
        PendingUnloadSubscription::TransitionInProgress
    ));
    let reservation = manager
        .thread_transition_reservation(thread_id)
        .await
        .expect("transition reservation should remain");
    assert_eq!(reservation.invalid_reason, None);
    assert_eq!(
        manager.caller_subscription(thread_id, newcomer).await,
        (false, 1)
    );

    manager
        .mark_thread_transition_prepared(thread_id, "transition-1", current_thread_id)
        .await
        .expect("transition should become prepared");
    assert!(
        manager
            .unsubscribe_connection_from_thread(thread_id, initiator)
            .await
    );
    assert!(matches!(
        manager
            .try_add_connection_to_thread_unless_pending_unload(
                &pending_thread_unloads,
                thread_id,
                initiator,
            )
            .await,
        PendingUnloadSubscription::Subscribed(())
    ));
    assert!(
        manager
            .thread_transition_reservation(thread_id)
            .await
            .is_none()
    );
    assert_eq!(
        manager.caller_subscription(thread_id, initiator).await,
        (true, 1)
    );
    Ok(())
}

#[tokio::test]
async fn listener_attach_rollback_removes_subscription_before_releasing_admission()
-> anyhow::Result<()> {
    let manager = ThreadStateManager::new();
    let pending_thread_unloads = Mutex::new(HashSet::new());
    let thread_id = ThreadId::new();
    let initiator = ConnectionId(1);
    let attaching_connection = ConnectionId(2);
    for connection_id in [initiator, attaching_connection] {
        manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
    }
    manager
        .try_ensure_connection_subscribed(
            thread_id, initiator, /*experimental_raw_events*/ false,
        )
        .await
        .expect("initiator should subscribe");
    let subscription = manager
        .try_ensure_connection_subscribed_unless_pending_unload(
            &pending_thread_unloads,
            thread_id,
            attaching_connection,
            /*experimental_raw_events*/ false,
        )
        .await;
    let ConversationSubscription::Subscribed {
        subscription_changed,
        transition_admission_permit,
        ..
    } = subscription
    else {
        panic!("listener connection should subscribe");
    };
    assert!(subscription_changed);
    assert_eq!(
        manager
            .reserve_thread_transition(thread_id, initiator, "racing-transition".to_string())
            .await,
        Err("outgoing_transition_conflict")
    );

    rollback_new_listener_subscription(
        &manager,
        thread_id,
        attaching_connection,
        transition_admission_permit,
    )
    .await;

    assert_eq!(
        manager
            .caller_subscription(thread_id, attaching_connection)
            .await,
        (false, 1)
    );
    manager
        .reserve_thread_transition(
            thread_id,
            initiator,
            "transition-after-rollback".to_string(),
        )
        .await
        .expect("rollback should release admission after removing the subscription");
    let reservation = manager
        .thread_transition_reservation(thread_id)
        .await
        .expect("transition should reserve after rollback");
    assert_eq!(reservation.invalid_reason, None);
    Ok(())
}
