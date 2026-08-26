use codex_thread_store::ThreadAccountRotationMode;
use codex_thread_store::ThreadAccountRotationPolicy;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn api_snapshot_preserves_policy_cursor_and_revision() {
    assert_eq!(
        api_snapshot(ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::RoundRobin,
            fixed_account_slot_id: Some("default".to_string()),
            automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string()],
            revision: 9,
            last_committed_account_slot_id: Some("secondary".to_string()),
        }),
        ThreadAccountRotationSnapshot {
            mode: ApiRotationMode::RoundRobin,
            fixed_account_slot_id: Some("default".to_string()),
            automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string()],
            revision: 9,
            last_committed_account_slot_id: Some("secondary".to_string()),
        }
    );
}

#[test]
fn invalid_thread_id_is_rejected_before_store_access() {
    assert!(parse_thread_id("not-a-thread-id").is_err());
}
