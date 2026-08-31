use codex_thread_store::ThreadAccountRotationMode;
use codex_thread_store::ThreadAccountRotationPolicy;
use codex_thread_store::ThreadAccountRotationPolicyRevision;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn api_thread_snapshot_separates_override_and_global_revisions() {
    assert_eq!(
        api_thread_snapshot(
            ThreadAccountRotationPolicy {
                mode: ThreadAccountRotationMode::RoundRobin,
                fixed_account_slot_id: None,
                automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
                revision: ThreadAccountRotationPolicyRevision::Override(9),
                last_committed_account_slot_id: Some("C2".to_string()),
            },
            Some(4),
        ),
        ThreadAccountRotationSnapshot {
            mode: ApiRotationMode::RoundRobin,
            fixed_account_slot_id: None,
            automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
            revision: 9,
            last_committed_account_slot_id: Some("C2".to_string()),
            source: ThreadAccountRotationSource::Override,
            global_profile_revision: Some(4),
        }
    );
}

#[test]
fn validation_fails_closed_and_sorts_registered_c_members() {
    let slots = vec![
        RotationSlotIdentity {
            account_slot_id: "C2".to_string(),
            account_number: 2,
        },
        RotationSlotIdentity {
            account_slot_id: "C1".to_string(),
            account_number: 1,
        },
    ];

    assert_eq!(
        validate_update(
            ApiRotationMode::RoundRobin,
            None,
            vec!["C2".to_string(), "C1".to_string()],
            &slots,
        )
        .unwrap(),
        AccountRotationProfileUpdate {
            mode: ThreadAccountRotationMode::RoundRobin,
            fixed_account_slot_id: None,
            automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
        }
    );
    for (mode, fixed, automatic) in [
        (
            ApiRotationMode::RoundRobin,
            None,
            vec!["C1".to_string(), "C1".to_string()],
        ),
        (ApiRotationMode::Fixed, Some("C3".to_string()), Vec::new()),
    ] {
        assert!(validate_update(mode, fixed, automatic, &slots).is_err());
    }
}

#[test]
fn invalid_thread_id_is_rejected_before_store_access() {
    assert!(parse_thread_id("not-a-thread-id").is_err());
}
