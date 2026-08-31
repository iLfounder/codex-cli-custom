use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::AccountBindingCommitIntent;
use super::SuccessfulAccountBindingTransition;
use super::SuccessfulAccountRotationCommit;
use super::ThreadAccountRotationPolicy;
use super::ThreadAccountRotationPolicyUpdate;
use crate::SqliteConfig;
use crate::StateRuntime;
use crate::ThreadAccountRotationMode;

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
async fn cursor_commit_is_fenced_by_binding_and_independent_of_profile_revision() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let binding = ExecutionAccountBinding {
        slot_id: "secondary".to_string(),
        generation: 3,
    };
    runtime
        .initialize_execution_account_binding(thread_id, &binding)
        .await
        .expect("initialize binding");
    runtime
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
        .await
        .expect("create override")
        .expect("revision zero override");
    runtime
        .compare_and_swap_thread_account_rotation_policy(thread_id, 1, &automatic_update())
        .await
        .expect("update override")
        .expect("revision one override");

    let committed = runtime
        .compare_and_swap_thread_account_rotation_cursor_for_binding(
            thread_id,
            &binding,
            "secondary",
        )
        .await
        .expect("commit cursor")
        .expect("matching binding");
    assert_eq!(committed.revision, 2);
    assert_eq!(
        committed.last_committed_account_slot_id,
        Some("secondary".to_string())
    );
    assert_eq!(
        runtime
            .compare_and_swap_thread_account_rotation_cursor_for_binding(
                thread_id,
                &ExecutionAccountBinding {
                    slot_id: binding.slot_id,
                    generation: binding.generation - 1,
                },
                "secondary",
            )
            .await
            .expect("reject stale binding"),
        None
    );
    runtime.close().await;
}

#[tokio::test]
async fn binding_commit_intent_preserves_or_atomically_pins_override() {
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
        .expect("create override")
        .expect("override commit");

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
async fn successful_rotation_atomically_advances_binding_and_cursor() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let initial = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 4,
    };
    runtime
        .initialize_execution_account_binding(thread_id, &initial)
        .await
        .expect("initialize binding");
    runtime
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
        .await
        .expect("create override")
        .expect("override commit");

    assert_eq!(
        runtime
            .compare_and_swap_successful_account_rotation(
                thread_id,
                &initial,
                "secondary",
                SuccessfulAccountBindingTransition::AdvanceGeneration,
            )
            .await
            .expect("commit successful rotation"),
        Some(SuccessfulAccountRotationCommit {
            binding: ExecutionAccountBinding {
                slot_id: "secondary".to_string(),
                generation: 5,
            },
            policy: ThreadAccountRotationPolicy {
                mode: ThreadAccountRotationMode::RoundRobin,
                fixed_account_slot_id: Some("default".to_string()),
                automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string(),],
                revision: 1,
                last_committed_account_slot_id: Some("secondary".to_string()),
            },
        })
    );
    runtime.close().await;
}

#[tokio::test]
async fn stale_binding_rolls_back_cursor_and_binding_transition() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let current = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 4,
    };
    runtime
        .initialize_execution_account_binding(thread_id, &current)
        .await
        .expect("initialize binding");

    assert_eq!(
        runtime
            .compare_and_swap_successful_account_rotation(
                thread_id,
                &ExecutionAccountBinding {
                    slot_id: "default".to_string(),
                    generation: 3,
                },
                "secondary",
                SuccessfulAccountBindingTransition::AdvanceGeneration,
            )
            .await
            .expect("reject stale binding"),
        None
    );
    assert_eq!(
        runtime
            .execution_account_binding(thread_id)
            .await
            .expect("read binding"),
        Some(current)
    );
    assert_eq!(
        runtime
            .thread_account_rotation_cursor(thread_id)
            .await
            .expect("read cursor"),
        None
    );
    runtime.close().await;
}

#[tokio::test]
async fn deleting_thread_removes_v5_and_v6_rotation_rows() {
    let (sqlite_home, runtime) = runtime().await;
    let _cleanup = scopeguard::guard(sqlite_home, |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let thread_id = ThreadId::default();
    let binding = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 1,
    };
    runtime
        .initialize_execution_account_binding(thread_id, &binding)
        .await
        .expect("initialize binding");
    runtime
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, &automatic_update())
        .await
        .expect("create override")
        .expect("override commit");
    runtime
        .compare_and_swap_thread_account_rotation_cursor_for_binding(thread_id, &binding, "default")
        .await
        .expect("create cursor")
        .expect("matching binding");

    runtime
        .delete_thread(thread_id)
        .await
        .expect("delete thread");

    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT (SELECT COUNT(*) FROM thread_account_rotation_policies WHERE thread_id = ?) \
             + (SELECT COUNT(*) FROM thread_account_rotation_overrides WHERE thread_id = ?) \
             + (SELECT COUNT(*) FROM thread_account_rotation_cursors WHERE thread_id = ?)",
        )
        .bind(thread_id.to_string())
        .bind(thread_id.to_string())
        .bind(thread_id.to_string())
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("count remaining rotation rows"),
        0
    );
    runtime.close().await;
}
