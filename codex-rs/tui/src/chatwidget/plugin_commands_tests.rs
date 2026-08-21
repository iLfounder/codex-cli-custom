use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use super::*;
use crate::app_command::AppCommand;
use crate::chatwidget::tests::make_chatwidget_manual;

#[tokio::test]
async fn plugin_prompt_with_shell_prefix_is_submitted_as_user_text() {
    let (mut chat, _event_rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    let prompt = "!echo plugin text".to_string();

    chat.submit_plugin_prompt(prompt.clone());

    let AppCommand::UserTurn { items, .. } = op_rx.try_recv().expect("user turn") else {
        panic!("plugin prompt must not become a local shell command");
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }]
    );
}
