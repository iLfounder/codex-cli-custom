use codex_app_server_protocol::RateLimitWindow;

use super::*;

fn candidate(number: u32, used: i32, resets_at: i64) -> RotationCandidate {
    RotationCandidate {
        account_slot_id: format!("slot-{number}"),
        account_number: number,
        ready: true,
        quota: RotationQuota::Fresh(
            Box::new(RateLimitSnapshot {
                limit_id: Some("codex".to_string()),
                limit_name: None,
                primary: Some(RateLimitWindow {
                    used_percent: used,
                    window_duration_mins: None,
                    resets_at: Some(resets_at),
                }),
                secondary: None,
                credits: None,
                individual_limit: None,
                spend_control_reached: None,
                plan_type: None,
                rate_limit_reached_type: None,
            }),
            HashMap::new(),
        ),
        hard_exhausted_hint: false,
    }
}

#[test]
fn quota_aware_uses_remaining_quota_per_reset_time() {
    let candidates = vec![candidate(2, 20, 200), candidate(3, 50, 120)];
    let membership = vec!["slot-2".to_string(), "slot-3".to_string()];
    assert_eq!(
        select_account(
            RotationSelectionRequest {
                mode: ThreadAccountRotationMode::QuotaAware,
                fixed_account_slot_id: None,
                automatic_account_slot_ids: &membership,
                current_account_slot_id: None,
                last_committed_account_slot_id: None,
                now: 100,
            },
            &candidates,
        ),
        RotationSelection::Selected("slot-3".to_string())
    );
}

#[test]
fn fixed_and_round_robin_obey_explicit_membership_rules() {
    let candidates = vec![candidate(2, 0, 200), candidate(3, 0, 200)];
    let membership = vec!["slot-2".to_string(), "slot-3".to_string()];
    assert_eq!(
        select_account(
            RotationSelectionRequest {
                mode: ThreadAccountRotationMode::Fixed,
                fixed_account_slot_id: Some("slot-3"),
                automatic_account_slot_ids: &[],
                current_account_slot_id: None,
                last_committed_account_slot_id: None,
                now: 100,
            },
            &candidates,
        ),
        RotationSelection::Selected("slot-3".to_string())
    );
    assert_eq!(
        select_account(
            RotationSelectionRequest {
                mode: ThreadAccountRotationMode::RoundRobin,
                fixed_account_slot_id: None,
                automatic_account_slot_ids: &membership,
                current_account_slot_id: Some("slot-2"),
                last_committed_account_slot_id: None,
                now: 100,
            },
            &candidates,
        ),
        RotationSelection::Selected("slot-3".to_string())
    );
}

#[test]
fn quota_aware_ignores_one_next_exhaustion_hint() {
    let mut hinted = candidate(2, 10, 200);
    hinted.hard_exhausted_hint = true;
    let candidates = vec![hinted, candidate(3, 90, 200)];
    let membership = vec!["slot-2".to_string(), "slot-3".to_string()];
    assert_eq!(
        select_account(
            RotationSelectionRequest {
                mode: ThreadAccountRotationMode::QuotaAware,
                fixed_account_slot_id: None,
                automatic_account_slot_ids: &membership,
                current_account_slot_id: None,
                last_committed_account_slot_id: None,
                now: 100,
            },
            &candidates,
        ),
        RotationSelection::Selected("slot-2".to_string())
    );
}

#[test]
fn automatic_same_target_still_selects_for_cursor_commit() {
    assert_eq!(
        selection_decision(
            ThreadAccountRotationMode::ExhaustThenNext,
            "slot-2".to_string(),
            "slot-2",
            7,
        ),
        TurnExecutionAccountDecision::Select {
            target_slot_id: "slot-2".to_string(),
            policy_revision: 7,
        }
    );
    assert_eq!(
        selection_decision(
            ThreadAccountRotationMode::Fixed,
            "slot-2".to_string(),
            "slot-2",
            7,
        ),
        TurnExecutionAccountDecision::Keep
    );
}
