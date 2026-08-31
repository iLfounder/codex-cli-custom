use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use crate::SqliteConfig;
use crate::StateRuntime;

#[tokio::test]
async fn binding_cas_and_turn_provenance_are_durable() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("create sqlite home");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let thread_id = ThreadId::default();
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("initialize runtime");

    let initial = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 1,
    };
    assert_eq!(
        runtime
            .initialize_execution_account_binding(thread_id, &initial)
            .await
            .expect("initialize binding"),
        initial
    );
    let next = runtime
        .compare_and_swap_execution_account_binding(thread_id, &initial, "secondary")
        .await
        .expect("compare and swap")
        .expect("matching generation");
    assert_eq!(
        next,
        ExecutionAccountBinding {
            slot_id: "secondary".to_string(),
            generation: 2,
        }
    );
    assert_eq!(
        runtime
            .compare_and_swap_execution_account_binding(thread_id, &initial, "stale")
            .await
            .expect("stale compare and swap"),
        None
    );

    runtime
        .record_turn_execution_account(thread_id, "turn-1", &next)
        .await
        .expect("record provenance");
    runtime
        .record_turn_execution_account(thread_id, "turn-1", &next)
        .await
        .expect("idempotent provenance");
    assert_eq!(
        runtime
            .turn_execution_account(thread_id, "turn-1")
            .await
            .expect("read provenance"),
        Some(next)
    );
    let err = runtime
        .record_turn_execution_account(thread_id, "turn-1", &initial)
        .await
        .expect_err("different provenance must not overwrite");
    assert!(err.to_string().contains("different execution account"));
    runtime.close().await;
}

#[tokio::test]
async fn account_slot_runtime_batch_cas_is_exact_atomic_and_legacy_versioned() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("create sqlite home");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let first_thread =
        ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
    let second_thread =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("thread id");
    let phantom_thread =
        ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("thread id");
    let other_thread =
        ThreadId::from_string("00000000-0000-0000-0000-000000000004").expect("thread id");
    for (thread_id, slot_id, generation) in [
        (first_thread, "secondary", 1),
        (second_thread, "secondary", 3),
        (other_thread, "other", 7),
    ] {
        runtime
            .initialize_execution_account_binding(
                thread_id,
                &ExecutionAccountBinding {
                    slot_id: slot_id.to_string(),
                    generation,
                },
            )
            .await
            .expect("initialize binding");
    }

    let legacy = runtime
        .execution_account_slot_runtime_state("secondary")
        .await
        .expect("read legacy slot runtime");
    assert_eq!(
        legacy,
        (
            0,
            vec![
                (
                    first_thread,
                    ExecutionAccountBinding {
                        slot_id: "secondary".to_string(),
                        generation: 1,
                    },
                ),
                (
                    second_thread,
                    ExecutionAccountBinding {
                        slot_id: "secondary".to_string(),
                        generation: 3,
                    },
                ),
            ],
        )
    );
    let committed = runtime
        .compare_and_swap_execution_account_slot_runtime("secondary", legacy.0, &legacy.1)
        .await
        .expect("commit slot runtime")
        .expect("exact snapshot should commit");
    assert_eq!(
        committed,
        (
            1,
            vec![
                (
                    first_thread,
                    ExecutionAccountBinding {
                        slot_id: "secondary".to_string(),
                        generation: 2,
                    },
                ),
                (
                    second_thread,
                    ExecutionAccountBinding {
                        slot_id: "secondary".to_string(),
                        generation: 4,
                    },
                ),
            ],
        )
    );
    assert_eq!(
        runtime
            .compare_and_swap_execution_account_slot_runtime("secondary", legacy.0, &legacy.1)
            .await
            .expect("stale commit should not fail"),
        None
    );

    let before_phantom = runtime
        .execution_account_slot_runtime_state("secondary")
        .await
        .expect("read current runtime");
    runtime
        .initialize_execution_account_binding(
            phantom_thread,
            &ExecutionAccountBinding {
                slot_id: "secondary".to_string(),
                generation: 1,
            },
        )
        .await
        .expect("insert phantom binding");
    assert_eq!(
        runtime
            .compare_and_swap_execution_account_slot_runtime(
                "secondary",
                before_phantom.0,
                &before_phantom.1,
            )
            .await
            .expect("phantom commit should not fail"),
        None
    );
    let before_overflow = runtime
        .execution_account_slot_runtime_state("secondary")
        .await
        .expect("read runtime after phantom");
    sqlx::query("UPDATE thread_execution_account_bindings SET generation = ? WHERE thread_id = ?")
        .bind(i64::MAX)
        .bind(phantom_thread.to_string())
        .execute(runtime.pool.as_ref())
        .await
        .expect("set overflow generation");
    let overflow_state = runtime
        .execution_account_slot_runtime_state("secondary")
        .await
        .expect("read overflow state");
    runtime
        .compare_and_swap_execution_account_slot_runtime(
            "secondary",
            overflow_state.0,
            &overflow_state.1,
        )
        .await
        .expect_err("generation overflow should reject the whole batch");
    assert_eq!(
        runtime
            .execution_account_slot_runtime_state("secondary")
            .await
            .expect("read state after overflow"),
        overflow_state
    );
    assert_eq!(overflow_state.0, before_overflow.0);
    runtime.close().await;
}
