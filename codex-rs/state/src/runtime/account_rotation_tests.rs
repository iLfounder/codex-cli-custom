use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::AccountBindingCommitIntent;
use super::ThreadAccountRotationMode;
use super::ThreadAccountRotationPolicy;
use super::ThreadAccountRotationPolicyUpdate;
use crate::SqliteConfig;
use crate::StateRuntime;

async fn runtime() -> (std::path::PathBuf, std::sync::Arc<StateRuntime>) {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("create sqlite home");
    let sqlite = SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("initialize runtime");
    (sqlite_home, runtime)
}

fn automatic_update() -> ThreadAccountRotationPolicyUpdate {
    ThreadAccountRotationPolicyUpdate {
        mode: ThreadAccountRotationMode::RoundRobin,
        fixed_account_slot_id: Some("default".to_string()),
        automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string()],
    }
}

#[tokio::test]
async fn policy_uses_virtual_revision_zero_then_exact_revision_and_cursor_cas() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let initial = ExecutionAccountBinding {
        slot_id: "secondary".to_string(),
        generation: 3,
    };
    runtime
        .initialize_execution_account_binding(thread_id, &initial)
        .await
        .expect("initialize binding");

    assert_eq!(
        runtime
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read virtual policy"),
        ThreadAccountRotationPolicy::virtual_fixed(&initial)
    );
    let committed = runtime
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
        .await
        .expect("create policy")
        .expect("virtual revision should commit");
    assert_eq!(
        committed,
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::RoundRobin,
            fixed_account_slot_id: Some("default".to_string()),
            automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string()],
            revision: 1,
            last_committed_account_slot_id: Some("secondary".to_string()),
        }
    );
    assert_eq!(
        runtime
            .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
            .await
            .expect("stale policy update"),
        None
    );

    let cursor = runtime
        .compare_and_swap_thread_account_rotation_cursor(thread_id, 1, "default")
        .await
        .expect("commit cursor")
        .expect("matching policy revision");
    assert_eq!(cursor.revision, 1);
    assert_eq!(
        cursor.last_committed_account_slot_id,
        Some("default".to_string())
    );
    assert_eq!(
        runtime
            .compare_and_swap_thread_account_rotation_cursor(thread_id, 0, "secondary")
            .await
            .expect("stale cursor update"),
        None
    );
    runtime.close().await;
}

#[tokio::test]
async fn binding_commit_intent_preserves_or_atomically_pins_rotation() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let initial = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 1,
    };
    runtime
        .initialize_execution_account_binding(thread_id, &initial)
        .await
        .expect("initialize binding");
    let policy = runtime
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
        .await
        .expect("create policy")
        .expect("policy commit");

    let automatic_binding = runtime
        .compare_and_swap_execution_account_binding_with_intent(
            thread_id,
            &initial,
            "secondary",
            AccountBindingCommitIntent::PreserveRotation,
        )
        .await
        .expect("automatic binding commit")
        .expect("matching binding");
    assert_eq!(
        runtime
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read preserved policy"),
        policy
    );

    let fixed_binding = runtime
        .compare_and_swap_execution_account_binding_with_intent(
            thread_id,
            &automatic_binding,
            "default",
            AccountBindingCommitIntent::PinFixed,
        )
        .await
        .expect("manual binding commit")
        .expect("matching binding");
    assert_eq!(fixed_binding.generation, 3);
    assert_eq!(
        runtime
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read fixed policy"),
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::Fixed,
            fixed_account_slot_id: Some("default".to_string()),
            automatic_account_slot_ids: policy.automatic_account_slot_ids,
            revision: 2,
            last_committed_account_slot_id: Some("default".to_string()),
        }
    );
    runtime.close().await;
}

#[tokio::test]
async fn deleting_thread_removes_rotation_policy() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    runtime
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
        .await
        .expect("create policy")
        .expect("policy commit");

    assert_eq!(
        runtime
            .delete_thread(thread_id)
            .await
            .expect("delete thread state"),
        0
    );
    assert_eq!(
        runtime
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read policy after delete"),
        ThreadAccountRotationPolicy::virtual_fixed(&ExecutionAccountBinding {
            slot_id: "default".to_string(),
            generation: 1,
        })
    );
    runtime.close().await;
}

#[tokio::test]
async fn credential_invalidation_removes_membership_from_every_policy() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let target = "replacement";
    let fixed_thread = ThreadId::new();
    let emptied_thread = ThreadId::new();
    let unaffected_thread = ThreadId::new();
    for thread_id in [fixed_thread, emptied_thread, unaffected_thread] {
        runtime
            .initialize_execution_account_binding(
                thread_id,
                &ExecutionAccountBinding {
                    slot_id: target.to_string(),
                    generation: 4,
                },
            )
            .await
            .expect("initialize binding");
    }
    let fixed = ThreadAccountRotationPolicyUpdate {
        mode: ThreadAccountRotationMode::Fixed,
        fixed_account_slot_id: Some(target.to_string()),
        automatic_account_slot_ids: vec![target.to_string(), "other".to_string()],
    };
    let emptied = ThreadAccountRotationPolicyUpdate {
        mode: ThreadAccountRotationMode::RoundRobin,
        fixed_account_slot_id: Some("other".to_string()),
        automatic_account_slot_ids: vec![target.to_string()],
    };
    let unaffected = ThreadAccountRotationPolicyUpdate {
        mode: ThreadAccountRotationMode::ExhaustThenNext,
        fixed_account_slot_id: Some(target.to_string()),
        automatic_account_slot_ids: vec!["other".to_string()],
    };
    for (thread_id, update) in [
        (fixed_thread, &fixed),
        (emptied_thread, &emptied),
        (unaffected_thread, &unaffected),
    ] {
        runtime
            .compare_and_swap_thread_account_rotation_policy(thread_id, 0, update)
            .await
            .expect("create policy")
            .expect("policy commit");
    }

    let mut affected = runtime
        .remove_account_slot_from_automatic_rotation_policies(target)
        .await
        .expect("remove automatic memberships");
    affected.sort_by_key(|(thread_id, _)| thread_id.to_string());
    let mut expected = vec![
        (
            fixed_thread,
            ThreadAccountRotationPolicy {
                mode: ThreadAccountRotationMode::Fixed,
                fixed_account_slot_id: Some(target.to_string()),
                automatic_account_slot_ids: vec!["other".to_string()],
                revision: 2,
                last_committed_account_slot_id: Some(target.to_string()),
            },
        ),
        (
            emptied_thread,
            ThreadAccountRotationPolicy {
                mode: ThreadAccountRotationMode::RoundRobin,
                fixed_account_slot_id: Some("other".to_string()),
                automatic_account_slot_ids: Vec::new(),
                revision: 2,
                last_committed_account_slot_id: Some(target.to_string()),
            },
        ),
    ];
    expected.sort_by_key(|(thread_id, _)| thread_id.to_string());
    assert_eq!(affected, expected);
    assert_eq!(
        runtime
            .thread_account_rotation_policy(unaffected_thread)
            .await
            .expect("read unaffected policy"),
        ThreadAccountRotationPolicy {
            mode: unaffected.mode,
            fixed_account_slot_id: unaffected.fixed_account_slot_id,
            automatic_account_slot_ids: unaffected.automatic_account_slot_ids,
            revision: 1,
            last_committed_account_slot_id: Some(target.to_string()),
        }
    );
    assert!(
        runtime
            .compare_and_swap_thread_account_rotation_policy(
                emptied_thread,
                2,
                &ThreadAccountRotationPolicyUpdate {
                    mode: ThreadAccountRotationMode::RoundRobin,
                    fixed_account_slot_id: None,
                    automatic_account_slot_ids: Vec::new(),
                },
            )
            .await
            .is_err()
    );
    runtime.close().await;
}

#[tokio::test]
async fn credential_invalidation_preflight_failure_has_zero_mutations() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let first_thread = ThreadId::new();
    let overflowing_thread = ThreadId::new();
    for thread_id in [first_thread, overflowing_thread] {
        runtime
            .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
            .await
            .expect("create policy")
            .expect("policy commit");
    }
    sqlx::query("UPDATE thread_account_rotation_policies SET revision = ? WHERE thread_id = ?")
        .bind(i64::MAX)
        .bind(overflowing_thread.to_string())
        .execute(runtime.pool.as_ref())
        .await
        .expect("seed overflowing revision");

    assert!(
        runtime
            .remove_account_slot_from_automatic_rotation_policies("secondary")
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .thread_account_rotation_policy(first_thread)
            .await
            .expect("read unchanged policy"),
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::RoundRobin,
            fixed_account_slot_id: Some("default".to_string()),
            automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string()],
            revision: 1,
            last_committed_account_slot_id: Some("default".to_string()),
        }
    );
    assert_eq!(
        runtime
            .thread_account_rotation_policy(overflowing_thread)
            .await
            .expect("read overflowing policy")
            .automatic_account_slot_ids,
        vec!["default".to_string(), "secondary".to_string()]
    );
    runtime.close().await;
}
