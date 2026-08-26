use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use crate::AccountBindingCommitIntent;
use crate::InMemoryThreadStore;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::ThreadAccountRotationMode;
use crate::ThreadAccountRotationPolicy;
use crate::ThreadAccountRotationPolicyUpdate;
use crate::ThreadStore;

fn automatic_update() -> ThreadAccountRotationPolicyUpdate {
    ThreadAccountRotationPolicyUpdate {
        mode: ThreadAccountRotationMode::RoundRobin,
        fixed_account_slot_id: Some("default".to_string()),
        automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string()],
    }
}

#[tokio::test]
async fn in_memory_rotation_matches_revision_cursor_and_binding_intent_contract() {
    let store = InMemoryThreadStore::default();
    let thread_id = ThreadId::default();
    let initial = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 1,
    };
    store
        .initialize_execution_account_binding(thread_id, initial.clone())
        .await
        .expect("initialize binding");
    assert_eq!(
        store
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read virtual policy"),
        ThreadAccountRotationPolicy::virtual_fixed(&initial)
    );

    let policy = store
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, automatic_update())
        .await
        .expect("create policy")
        .expect("matching revision");
    assert_eq!(policy.revision, 1);
    assert_eq!(
        store
            .compare_and_swap_thread_account_rotation_cursor(thread_id, 1, "secondary".to_string(),)
            .await
            .expect("commit cursor")
            .expect("matching policy revision")
            .last_committed_account_slot_id,
        Some("secondary".to_string())
    );

    let automatic_binding = store
        .compare_and_swap_execution_account_binding_with_intent(
            thread_id,
            initial,
            "secondary".to_string(),
            AccountBindingCommitIntent::PreserveRotation,
        )
        .await
        .expect("automatic binding commit")
        .expect("matching binding");
    assert_eq!(
        store
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read preserved policy")
            .mode,
        ThreadAccountRotationMode::RoundRobin
    );

    store
        .compare_and_swap_execution_account_binding(
            thread_id,
            automatic_binding,
            "default".to_string(),
        )
        .await
        .expect("legacy manual binding commit")
        .expect("matching binding");
    assert_eq!(
        store
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read pinned policy"),
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::Fixed,
            fixed_account_slot_id: Some("default".to_string()),
            automatic_account_slot_ids: policy.automatic_account_slot_ids,
            revision: 2,
            last_committed_account_slot_id: Some("default".to_string()),
        }
    );
}

#[tokio::test]
async fn local_legacy_binding_commit_atomically_pins_policy() {
    let home = tempfile::TempDir::new().expect("temp dir");
    let sqlite = codex_state::SqliteConfig::new_for_testing(home.path().abs());
    let runtime = codex_state::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize state runtime");
    let store = LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: home.path().to_path_buf(),
            sqlite,
            default_model_provider_id: "test-provider".to_string(),
        },
        Some(runtime.clone()),
    );
    let thread_id = ThreadId::default();
    let initial = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 1,
    };
    store
        .initialize_execution_account_binding(thread_id, initial.clone())
        .await
        .expect("initialize binding");
    store
        .compare_and_swap_thread_account_rotation_policy(thread_id, 0, automatic_update())
        .await
        .expect("create policy")
        .expect("matching revision");

    assert_eq!(
        store
            .compare_and_swap_execution_account_binding(
                thread_id,
                initial,
                "secondary".to_string(),
            )
            .await
            .expect("legacy binding commit"),
        Some(ExecutionAccountBinding {
            slot_id: "secondary".to_string(),
            generation: 2,
        })
    );
    assert_eq!(
        store
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read pinned policy"),
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::Fixed,
            fixed_account_slot_id: Some("secondary".to_string()),
            automatic_account_slot_ids: vec!["default".to_string(), "secondary".to_string()],
            revision: 2,
            last_committed_account_slot_id: Some("secondary".to_string()),
        }
    );
    assert_eq!(
        store
            .remove_account_slot_from_automatic_rotation_policies("secondary".to_string())
            .await
            .expect("remove automatic membership"),
        vec![(
            thread_id,
            ThreadAccountRotationPolicy {
                mode: ThreadAccountRotationMode::Fixed,
                fixed_account_slot_id: Some("secondary".to_string()),
                automatic_account_slot_ids: vec!["default".to_string()],
                revision: 3,
                last_committed_account_slot_id: Some("secondary".to_string()),
            },
        )]
    );
    runtime.close().await;
}

#[tokio::test]
async fn in_memory_credential_invalidation_matches_all_policy_semantics() {
    let store = InMemoryThreadStore::default();
    let target = "replacement";
    let fixed_thread = ThreadId::new();
    let emptied_thread = ThreadId::new();
    let unaffected_thread = ThreadId::new();
    let updates = [
        (
            fixed_thread,
            ThreadAccountRotationPolicyUpdate {
                mode: ThreadAccountRotationMode::Fixed,
                fixed_account_slot_id: Some(target.to_string()),
                automatic_account_slot_ids: vec![target.to_string(), "other".to_string()],
            },
        ),
        (
            emptied_thread,
            ThreadAccountRotationPolicyUpdate {
                mode: ThreadAccountRotationMode::QuotaAware,
                fixed_account_slot_id: Some("other".to_string()),
                automatic_account_slot_ids: vec![target.to_string()],
            },
        ),
        (
            unaffected_thread,
            ThreadAccountRotationPolicyUpdate {
                mode: ThreadAccountRotationMode::RoundRobin,
                fixed_account_slot_id: Some(target.to_string()),
                automatic_account_slot_ids: vec!["other".to_string()],
            },
        ),
    ];
    for (thread_id, update) in updates {
        store
            .initialize_execution_account_binding(
                thread_id,
                ExecutionAccountBinding {
                    slot_id: target.to_string(),
                    generation: 7,
                },
            )
            .await
            .expect("initialize binding");
        store
            .compare_and_swap_thread_account_rotation_policy(thread_id, 0, update)
            .await
            .expect("create policy")
            .expect("policy commit");
    }

    let affected = store
        .remove_account_slot_from_automatic_rotation_policies(target.to_string())
        .await
        .expect("remove automatic memberships");
    assert_eq!(affected.len(), 2);
    assert_eq!(
        store
            .thread_account_rotation_policy(fixed_thread)
            .await
            .expect("read fixed policy"),
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::Fixed,
            fixed_account_slot_id: Some(target.to_string()),
            automatic_account_slot_ids: vec!["other".to_string()],
            revision: 2,
            last_committed_account_slot_id: Some(target.to_string()),
        }
    );
    assert_eq!(
        store
            .thread_account_rotation_policy(emptied_thread)
            .await
            .expect("read emptied policy"),
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::QuotaAware,
            fixed_account_slot_id: Some("other".to_string()),
            automatic_account_slot_ids: Vec::new(),
            revision: 2,
            last_committed_account_slot_id: Some(target.to_string()),
        }
    );
    assert_eq!(
        store
            .thread_account_rotation_policy(unaffected_thread)
            .await
            .expect("read unaffected policy")
            .revision,
        1
    );
}
