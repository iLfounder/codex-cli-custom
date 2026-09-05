use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use crate::AccountRotationProfileUpdate;
use crate::InMemoryThreadStore;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::SuccessfulAccountBindingTransition;
use crate::ThreadAccountRotationMode;
use crate::ThreadAccountRotationPolicy;
use crate::ThreadAccountRotationPolicyRevision;
use crate::ThreadStore;

fn automatic_update(slot_ids: &[&str]) -> AccountRotationProfileUpdate {
    AccountRotationProfileUpdate {
        mode: ThreadAccountRotationMode::RoundRobin,
        fixed_account_slot_id: None,
        automatic_account_slot_ids: slot_ids.iter().map(ToString::to_string).collect(),
    }
}

fn initial_binding() -> ExecutionAccountBinding {
    ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 1,
    }
}

#[tokio::test]
async fn in_memory_effective_policy_distinguishes_inherit_and_override() {
    let store = InMemoryThreadStore::default();
    let inherited_thread = ThreadId::new();
    let overridden_thread = ThreadId::new();
    let binding = initial_binding();
    for thread_id in [inherited_thread, overridden_thread] {
        store
            .initialize_execution_account_binding(thread_id, binding.clone())
            .await
            .expect("initialize binding");
    }

    assert_eq!(
        store
            .thread_account_rotation_policy(inherited_thread)
            .await
            .expect("read pre-activation policy"),
        ThreadAccountRotationPolicy::virtual_fixed(&binding)
    );
    store
        .compare_and_swap_account_rotation_global_profile(
            /*expected_revision*/ 0,
            automatic_update(&["default", "secondary"]),
        )
        .await
        .expect("activate global profile")
        .expect("global revision matches");
    store
        .compare_and_swap_thread_account_rotation_override(
            overridden_thread,
            /*expected_revision*/ 0,
            AccountRotationProfileUpdate {
                mode: ThreadAccountRotationMode::Fixed,
                fixed_account_slot_id: Some("override".to_string()),
                automatic_account_slot_ids: Vec::new(),
            },
        )
        .await
        .expect("create override")
        .expect("override revision matches");

    let inherited = store
        .thread_account_rotation_policy(inherited_thread)
        .await
        .expect("read inherited policy");
    let overridden = store
        .thread_account_rotation_policy(overridden_thread)
        .await
        .expect("read overridden policy");
    assert_eq!(
        (inherited.revision, overridden.revision),
        (
            ThreadAccountRotationPolicyRevision::Inherit(1),
            ThreadAccountRotationPolicyRevision::Override(1),
        )
    );

    store
        .compare_and_swap_account_rotation_global_profile(
            /*expected_revision*/ 1,
            automatic_update(&["secondary", "third"]),
        )
        .await
        .expect("update global profile")
        .expect("global revision matches");
    assert_eq!(
        store
            .thread_account_rotation_policy(overridden_thread)
            .await
            .expect("override survives global update"),
        overridden
    );
    assert!(
        !store
            .reset_thread_account_rotation_override(overridden_thread, /*expected_revision*/ 2)
            .await
            .expect("stale reset")
    );
    assert!(
        store
            .reset_thread_account_rotation_override(overridden_thread, /*expected_revision*/ 1)
            .await
            .expect("exact reset")
    );
    assert_eq!(
        store
            .thread_account_rotation_policy(overridden_thread)
            .await
            .expect("read inherited policy after reset")
            .revision,
        ThreadAccountRotationPolicyRevision::Inherit(2)
    );
}

#[tokio::test]
async fn in_memory_cursor_and_success_commit_are_fenced_only_by_exact_binding() {
    let store = InMemoryThreadStore::default();
    let thread_id = ThreadId::new();
    let initial = initial_binding();
    store
        .initialize_execution_account_binding(thread_id, initial.clone())
        .await
        .expect("initialize binding");
    store
        .compare_and_swap_account_rotation_global_profile(
            /*expected_revision*/ 0,
            automatic_update(&["default", "secondary"]),
        )
        .await
        .expect("activate global profile")
        .expect("global revision matches");

    store
        .compare_and_swap_account_rotation_global_profile(
            /*expected_revision*/ 1,
            automatic_update(&["secondary", "default"]),
        )
        .await
        .expect("update global after turn snapshot")
        .expect("global revision matches");
    let committed = store
        .compare_and_swap_successful_account_rotation(
            thread_id,
            initial.clone(),
            "secondary".to_string(),
            SuccessfulAccountBindingTransition::AdvanceGeneration,
        )
        .await
        .expect("commit successful selection")
        .expect("exact binding matches");
    assert_eq!(
        committed.binding,
        ExecutionAccountBinding {
            slot_id: "secondary".to_string(),
            generation: 2,
        }
    );
    assert_eq!(
        store
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read cursor after commit"),
        ThreadAccountRotationPolicy {
            mode: ThreadAccountRotationMode::RoundRobin,
            fixed_account_slot_id: None,
            automatic_account_slot_ids: vec!["secondary".to_string(), "default".to_string()],
            revision: ThreadAccountRotationPolicyRevision::Inherit(2),
            last_committed_account_slot_id: Some("secondary".to_string()),
        }
    );
    assert_eq!(
        store
            .compare_and_swap_thread_account_rotation_cursor_for_binding(
                thread_id,
                initial,
                "default".to_string(),
            )
            .await
            .expect("stale cursor commit is not an error"),
        None
    );
}

#[tokio::test]
async fn local_store_matches_effective_profile_and_binding_cursor_contract() {
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
    let thread_id = ThreadId::new();
    let binding = initial_binding();
    store
        .initialize_execution_account_binding(thread_id, binding.clone())
        .await
        .expect("initialize binding");
    store
        .compare_and_swap_account_rotation_global_profile(
            /*expected_revision*/ 0,
            automatic_update(&["default", "secondary"]),
        )
        .await
        .expect("activate global profile")
        .expect("global revision matches");

    assert_eq!(
        store
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read inherited local policy")
            .revision,
        ThreadAccountRotationPolicyRevision::Inherit(1)
    );
    store
        .compare_and_swap_thread_account_rotation_cursor_for_binding(
            thread_id,
            binding.clone(),
            binding.slot_id.clone(),
        )
        .await
        .expect("commit local cursor")
        .expect("exact binding matches");
    assert_eq!(
        store
            .thread_account_rotation_policy(thread_id)
            .await
            .expect("read committed local cursor")
            .last_committed_account_slot_id,
        Some(binding.slot_id)
    );
    runtime.close().await;
}
