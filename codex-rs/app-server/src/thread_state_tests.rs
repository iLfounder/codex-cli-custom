use super::*;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use pretty_assertions::assert_eq;

fn turn_started(turn_id: &str) -> EventMsg {
    EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    })
}

fn turn_aborted(turn_id: &str) -> EventMsg {
    EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some(turn_id.to_string()),
        reason: TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    })
}

#[test]
fn admitted_turn_is_interruptible_until_matching_publication_or_terminal() {
    let mut state = ThreadState::default();

    state.register_admitted_turn("turn-1".to_string());
    assert_eq!(state.interruptible_turn_id(), Some("turn-1"));

    state.track_current_turn_event("older-turn", &turn_started("older-turn"));
    assert_eq!(state.interruptible_turn_id(), Some("turn-1"));
    state.track_current_turn_event("older-turn", &turn_aborted("older-turn"));
    assert_eq!(state.interruptible_turn_id(), Some("turn-1"));

    state.track_current_turn_event("turn-1", &turn_started("turn-1"));
    assert_eq!(state.interruptible_turn_id(), Some("turn-1"));

    state.track_current_turn_event("turn-1", &turn_aborted("turn-1"));
    assert_eq!(state.interruptible_turn_id(), None);
}

#[test]
fn publication_before_registration_does_not_leave_a_provisional_turn() {
    let mut state = ThreadState::default();
    state.track_current_turn_event("turn-1", &turn_started("turn-1"));

    state.register_admitted_turn("turn-1".to_string());

    assert_eq!(state.admitted_turn_id, None);
    assert_eq!(state.interruptible_turn_id(), Some("turn-1"));
}

#[test]
fn terminal_before_registration_rejects_the_completed_turn() {
    let mut state = ThreadState::default();
    state.track_current_turn_event("turn-1", &turn_started("turn-1"));
    state.track_current_turn_event("turn-1", &turn_aborted("turn-1"));

    state.register_admitted_turn("turn-1".to_string());

    assert_eq!(state.admitted_turn_id, None);
    assert_eq!(state.interruptible_turn_id(), None);
}

#[test]
fn listener_clear_discards_an_unpublished_admission() {
    let mut state = ThreadState::default();
    state.register_admitted_turn("turn-1".to_string());

    state.clear_listener();

    assert_eq!(state.interruptible_turn_id(), None);
}
