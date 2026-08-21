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
