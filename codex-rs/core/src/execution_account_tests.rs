use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn default_turn_execution_account_selector_keeps_current_binding() {
    let selector = DefaultTurnExecutionAccountSelector;
    let selection = TurnExecutionAccountSelection {
        thread_id: ThreadId::new(),
        current_binding: ExecutionAccountBinding {
            slot_id: "default".to_string(),
            generation: 1,
        },
    };

    assert_eq!(
        selector.select(selection).await.expect("select account"),
        TurnExecutionAccountDecision::Keep
    );
}
