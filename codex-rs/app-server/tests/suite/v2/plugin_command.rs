use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::PluginCommandInvokeParams;
use codex_app_server_protocol::PluginCommandInvokeResponse;
use codex_app_server_protocol::PluginCommandListParams;
use codex_app_server_protocol::PluginCommandListResponse;
use codex_app_server_protocol::ThreadPresentation;
use codex_app_server_protocol::ThreadPresentationAppendParams;
use codex_app_server_protocol::ThreadPresentationAppendResponse;
use codex_app_server_protocol::ThreadPresentationAppendedNotification;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn plugin_prompt_invocation_and_presentation_are_thread_scoped_and_ephemeral() -> Result<()> {
    let codex_home = TempDir::new()?;
    let plugin_root = codex_home
        .path()
        .join("plugins/cache/personal/fixture/local/.codex-plugin");
    std::fs::create_dir_all(&plugin_root)?;
    std::fs::write(
        plugin_root.join("plugin.json"),
        r#"{
          "name":"fixture",
          "contributions":{"commands":[{
            "id":"review","name":"review","description":"Review",
            "target":{"type":"prompt","prompt":"Review the current change."}
          },{
            "id":"goal","name":"goal","description":"Read the goal",
            "target":{"type":"action","action":"goalGet"}
          }]}
        }"#,
    )?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"[features]
plugins = true
goals = true

[plugins."fixture@personal"]
enabled = true
"#,
    )?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let started = app.start_thread(ThreadStartParams::default()).await?;

    let catalog: PluginCommandListResponse = app
        .request(|request_id| ClientRequest::PluginCommandList {
            request_id,
            params: PluginCommandListParams {
                thread_id: started.thread.id.clone(),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let mut commands = catalog.data.into_iter();
    let goal_command = commands.next().expect("goal command");
    let command = commands.next().expect("prompt command");
    assert_eq!(goal_command.canonical_name, "/fixture:goal");
    assert_eq!(command.canonical_name, "/fixture:review");
    assert_eq!(command.short_name.as_deref(), Some("/review"));
    let invoked: PluginCommandInvokeResponse = app
        .request(|request_id| ClientRequest::PluginCommandInvoke {
            request_id,
            params: PluginCommandInvokeParams {
                thread_id: started.thread.id.clone(),
                command_id: command.id,
            },
        })
        .await?;
    assert_eq!(
        invoked,
        PluginCommandInvokeResponse::Prompt {
            prompt: "Review the current change.".to_string()
        }
    );
    let goal: PluginCommandInvokeResponse = app
        .request(|request_id| ClientRequest::PluginCommandInvoke {
            request_id,
            params: PluginCommandInvokeParams {
                thread_id: started.thread.id.clone(),
                command_id: goal_command.id,
            },
        })
        .await?;
    assert_eq!(goal, PluginCommandInvokeResponse::GoalGet { goal: None });

    let before: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: started.thread.id.clone(),
                include_turns: false,
            },
        })
        .await?;
    let response: ThreadPresentationAppendResponse = app
        .request(|request_id| ClientRequest::ThreadPresentationAppend {
            request_id,
            params: ThreadPresentationAppendParams {
                thread_id: started.thread.id.clone(),
                item: ThreadPresentation::Notice {
                    id: "relay-status".to_string(),
                    level: codex_app_server_protocol::ThreadPresentationNoticeLevel::Info,
                    message: "Relay message accepted".to_string(),
                },
            },
        })
        .await?;
    assert_eq!(response.delivered_to, 1);
    let notification: ThreadPresentationAppendedNotification = app
        .read_notification("thread/presentation/appended")
        .await?;
    assert_eq!(notification.thread_id, started.thread.id);
    let after: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: notification.thread_id,
                include_turns: false,
            },
        })
        .await?;
    assert_eq!(after, before);
    Ok(())
}
