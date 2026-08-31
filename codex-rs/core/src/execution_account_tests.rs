use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn default_turn_execution_account_selector_keeps_current_binding() {
    let selector = DefaultTurnExecutionAccountSelector;
    let current_binding = ExecutionAccountBinding {
        slot_id: "default".to_string(),
        generation: 1,
    };
    let selection = TurnExecutionAccountSelection {
        thread_id: ThreadId::new(),
        current_binding: current_binding.clone(),
        account_rotation_policy: codex_thread_store::ThreadAccountRotationPolicy::virtual_fixed(
            &current_binding,
        ),
        credential_revision: None,
    };

    assert_eq!(
        selector.select(selection).await.expect("select account"),
        TurnExecutionAccountDecision::Keep
    );
}
