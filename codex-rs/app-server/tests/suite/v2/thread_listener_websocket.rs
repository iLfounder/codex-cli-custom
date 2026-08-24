use anyhow::Context;
use anyhow::Result;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::SessionRuntimeChangedNotification;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeListParams;
use codex_app_server_protocol::SessionRuntimeListResponse;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use std::fs::OpenOptions;
use std::io::Write;
use tempfile::TempDir;
use tokio::time::timeout;

use super::connection_handling_websocket::DEFAULT_READ_TIMEOUT;
use super::connection_handling_websocket::connect_websocket;
use super::connection_handling_websocket::create_config_toml;
use super::connection_handling_websocket::read_notification_for_method;
use super::connection_handling_websocket::read_response_for_id;
use super::connection_handling_websocket::send_initialize_request;
use super::connection_handling_websocket::send_request;
use super::connection_handling_websocket::spawn_websocket_server;

#[tokio::test]
async fn disconnected_cold_resume_rolls_back_exact_thread_and_writer() -> Result<()> {
    let responses = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &responses.uri(), "never")?;
    let cold_thread_id = create_fake_rollout(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "cold resume disconnect",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let initialize_barrier = codex_home.path().join("allow-mcp-initialize");
    std::fs::write(&initialize_barrier, "ready")?;
    let mut config = OpenOptions::new()
        .append(true)
        .open(codex_home.path().join("config.toml"))?;
    writeln!(
        config,
        r#"
[mcp_servers.listener-stall]
command = {}
required = true
startup_timeout_sec = 60

[mcp_servers.listener-stall.env]
MCP_TEST_INITIALIZE_BARRIER_FILE = {}
"#,
        toml::Value::String(core_test_support::stdio_server_bin()?),
        toml::Value::String(initialize_barrier.to_string_lossy().into_owned()),
    )?;
    drop(config);

    let (mut process, bind_addr) = spawn_websocket_server(codex_home.path()).await?;
    let mut requester = connect_websocket(bind_addr).await?;
    let mut observer = connect_websocket(bind_addr).await?;
    send_initialize_request(&mut requester, /*id*/ 1, "resume-requester").await?;
    read_response_for_id(&mut requester, /*id*/ 1).await?;
    send_initialize_request(&mut observer, /*id*/ 1, "runtime-observer").await?;
    read_response_for_id(&mut observer, /*id*/ 1).await?;

    send_request(
        &mut requester,
        "thread/start",
        /*id*/ 2,
        Some(serde_json::to_value(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })?),
    )
    .await?;
    let throwaway: ThreadStartResponse =
        to_response(read_response_for_id(&mut requester, /*id*/ 2).await?)?;
    std::fs::remove_file(&initialize_barrier)?;

    send_request(
        &mut requester,
        "thread/resume",
        /*id*/ 3,
        Some(serde_json::to_value(ThreadResumeParams {
            thread_id: cold_thread_id.clone(),
            ..Default::default()
        })?),
    )
    .await?;
    requester
        .close(None)
        .await
        .context("failed to close resume requester websocket")?;

    let throwaway_after_disconnect = timeout(DEFAULT_READ_TIMEOUT, async {
        let mut request_id = 10;
        loop {
            send_request(
                &mut observer,
                "sessionRuntime/list",
                request_id,
                Some(serde_json::to_value(SessionRuntimeListParams {
                    cursor: None,
                    limit: None,
                    thread_id: Some(throwaway.thread.id.clone()),
                })?),
            )
            .await?;
            let response: SessionRuntimeListResponse =
                to_response(read_response_for_id(&mut observer, request_id).await?)?;
            request_id += 1;
            if let Some(snapshot) = response.data.into_iter().next()
                && snapshot.lifecycle.subscriber_count == 0
            {
                return Ok::<_, anyhow::Error>(snapshot);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .context("timed out waiting for requester disconnect observation")??;
    assert_eq!(throwaway_after_disconnect.lifecycle.subscriber_count, 0);

    std::fs::write(&initialize_barrier, "ready")?;
    let rolled_back = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let notification =
                read_notification_for_method(&mut observer, "sessionRuntime/changed").await?;
            let changed: SessionRuntimeChangedNotification = serde_json::from_value(
                notification
                    .params
                    .context("sessionRuntime/changed params")?,
            )?;
            if changed.snapshot.thread_id == cold_thread_id
                && changed.snapshot.lifecycle.state == SessionRuntimeLifecycleState::NotLoaded
                && changed.snapshot.writer.state == SessionRuntimeWriterState::None
            {
                return Ok::<_, anyhow::Error>(changed.snapshot);
            }
        }
    })
    .await
    .context("timed out waiting for completed cold-resume rollback")??;
    assert_eq!(
        (rolled_back.lifecycle.state, rolled_back.writer.state),
        (
            SessionRuntimeLifecycleState::NotLoaded,
            SessionRuntimeWriterState::None,
        )
    );

    send_request(
        &mut observer,
        "thread/resume",
        /*id*/ 100,
        Some(serde_json::to_value(ThreadResumeParams {
            thread_id: cold_thread_id.clone(),
            ..Default::default()
        })?),
    )
    .await?;
    let resumed: ThreadResumeResponse =
        to_response(read_response_for_id(&mut observer, /*id*/ 100).await?)?;
    assert_eq!(resumed.thread.id, cold_thread_id);

    process
        .kill()
        .await
        .context("failed to stop websocket app-server process")?;
    Ok(())
}
