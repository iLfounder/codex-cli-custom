use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::AccountSlotChangedNotification;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeOperationUpdatedNotification;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn api_key_login_creates_ready_private_slot_and_sanitized_events() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"
cli_auth_credentials_store = "file"

[features]
shell_snapshot = false
"#,
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = app_server
        .send_raw_request(
            "accountSlot/login/start",
            Some(serde_json::to_value(AccountSlotLoginStartParams::ApiKey {
                slot_id: None,
                api_key: "test-slot-secret".to_string(),
            })?),
        )
        .await?;
    let response: AccountSlotLoginStartResponse =
        timeout(DEFAULT_TIMEOUT, app_server.read_response(request_id)).await??;
    let accepted: SessionRuntimeOperationUpdatedNotification = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_notification("sessionRuntime/operation/updated"),
    )
    .await??;
    let changed: AccountSlotChangedNotification = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_notification("accountSlot/changed"),
    )
    .await??;
    let ready: SessionRuntimeOperationUpdatedNotification = timeout(
        DEFAULT_TIMEOUT,
        app_server.read_notification("sessionRuntime/operation/updated"),
    )
    .await??;

    assert_eq!(response.slot.status, AccountSlotStatus::Ready);
    assert_eq!(response.challenge, None);
    assert_eq!(changed.slot, response.slot);
    assert_eq!(
        accepted.operation.status,
        SessionRuntimeOperationStatus::Accepted
    );
    assert_eq!(ready.operation, response.operation);
    let response_json = serde_json::to_string(&response)?;
    assert!(!response_json.contains("test-slot-secret"));
    assert!(!response_json.contains(codex_home.path().to_string_lossy().as_ref()));
    Ok(())
}
