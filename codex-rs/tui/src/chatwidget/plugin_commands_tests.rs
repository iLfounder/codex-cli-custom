use codex_app_server_protocol::SessionRuntimeAccountRef;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use crate::app_command::AppCommand;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;

#[tokio::test]
async fn plugin_prompt_with_shell_prefix_is_submitted_as_user_text() {
    let (mut chat, _app_event_tx, _event_rx, mut op_rx) =
        make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(ThreadId::new());
    let prompt = "!echo plugin text".to_string();
    let account = SessionRuntimeAccountRef {
        account_slot_id: "slot-1".to_string(),
        execution_generation: 7,
    };

    chat.submit_plugin_prompt(prompt.clone(), account.clone());

    let AppCommand::UserTurn {
        items,
        expected_execution_account,
        ..
    } = op_rx.try_recv().expect("user turn")
    else {
        panic!("plugin prompt must not become a local shell command");
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }]
    );
    assert_eq!(expected_execution_account, Some(account));
}

#[tokio::test]
async fn plugin_prompt_account_survives_pending_steer_retry() {
    let (mut chat, _app_event_tx, _event_rx, mut op_rx) =
        make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(ThreadId::new());
    chat.on_task_started();
    let account = SessionRuntimeAccountRef {
        account_slot_id: "slot-1".to_string(),
        execution_generation: 7,
    };

    chat.submit_plugin_prompt("review".to_string(), account.clone());
    let AppCommand::UserTurn {
        expected_execution_account,
        ..
    } = op_rx.try_recv().expect("steer user turn")
    else {
        panic!("plugin prompt must submit a user turn");
    };
    assert_eq!(expected_execution_account, Some(account.clone()));
    assert_eq!(
        chat.input_queue
            .pending_steers
            .front()
            .and_then(|pending| pending.history_record.expected_account()),
        Some(&account)
    );

    assert!(chat.enqueue_rejected_steer());
    let (queued, history_record) = chat
        .pop_next_queued_user_message()
        .expect("rejected steer retry");
    assert_eq!(history_record.expected_account(), Some(&account));
    assert!(
        chat.submit_user_message_with_history_record(queued.into_user_message(), history_record,)
    );

    let AppCommand::UserTurn {
        expected_execution_account,
        ..
    } = op_rx.try_recv().expect("retried user turn")
    else {
        panic!("plugin prompt retry must remain a user turn");
    };
    assert_eq!(expected_execution_account, Some(account));
}

#[tokio::test]
async fn merged_plugin_prompts_do_not_retry_without_one_exact_account() {
    let (mut chat, _app_event_tx, _event_rx, mut op_rx) =
        make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(ThreadId::new());
    chat.on_task_started();
    for (slot, generation) in [("slot-1", 7), ("slot-2", 9)] {
        chat.submit_plugin_prompt(
            "review".to_string(),
            SessionRuntimeAccountRef {
                account_slot_id: slot.to_string(),
                execution_generation: generation,
            },
        );
        op_rx.try_recv().expect("initial steer user turn");
        assert!(chat.enqueue_rejected_steer());
    }

    let (queued, history_record) = chat
        .pop_next_queued_user_message()
        .expect("merged rejected steers");
    assert!(history_record.has_account_conflict());
    assert!(
        !chat.submit_user_message_with_history_record(queued.into_user_message(), history_record,)
    );
    assert!(op_rx.try_recv().is_err());
}
