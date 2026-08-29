use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::AccountRotationChangedNotification;
use codex_app_server_protocol::AccountRotationReadResponse;
use codex_app_server_protocol::AccountRotationSnapshot;
use codex_app_server_protocol::AccountRotationUpdateParams;
use codex_app_server_protocol::AccountRotationUpdateResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadAccountRotationMode;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn global_rotation_create_cas_and_notification_use_public_rpc() -> Result<()> {
    let owner_home = TempDir::new()?;
    let c1_home = owner_home.path().join("account1");
    let c2_home = owner_home.path().join("account2");
    std::fs::create_dir_all(&c1_home)?;
    std::fs::create_dir_all(&c2_home)?;
    std::fs::write(c1_home.join("config.toml"), "model = \"mock-model\"\n")?;
    write_owner_catalog(owner_home.path(), &[(&c1_home, 1), (&c2_home, 2)])?;

    let token_manager = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshots"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accounts": [],
        })))
        .mount(&token_manager)
        .await;
    let endpoint = format!("{}/", token_manager.uri());
    let owner_home_env = owner_home.path().to_string_lossy();
    let mut app = TestAppServer::builder()
        .with_codex_home(&c1_home)
        .without_auto_env()
        .with_env_overrides(&[
            ("HOME", Some(owner_home_env.as_ref())),
            (
                "CODEX_APP_SERVER_TEST_TOKEN_MANAGER_URL",
                Some(endpoint.as_str()),
            ),
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let read_id = app.send_raw_request("accountRotation/read", None).await?;
    let initial: AccountRotationReadResponse = app.read_response(read_id).await?;
    assert_eq!(initial.rotation, None);

    let update = AccountRotationUpdateParams {
        expected_rotation_revision: 0,
        mode: ThreadAccountRotationMode::RoundRobin,
        fixed_account_slot_id: None,
        automatic_account_slot_ids: vec!["C2".to_string(), "C1".to_string()],
    };
    let update_id = app
        .send_raw_request(
            "accountRotation/update",
            Some(serde_json::to_value(update)?),
        )
        .await?;
    let updated: AccountRotationUpdateResponse = app.read_response(update_id).await?;
    let expected = AccountRotationSnapshot {
        mode: ThreadAccountRotationMode::RoundRobin,
        fixed_account_slot_id: None,
        automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
        revision: 1,
    };
    assert_eq!(updated.rotation, expected);

    let changed: AccountRotationChangedNotification = tokio::time::timeout(
        DEFAULT_TIMEOUT,
        app.read_notification("accountRotation/changed"),
    )
    .await??;
    assert_eq!(changed.rotation, expected);

    let stale_id = app
        .send_raw_request(
            "accountRotation/update",
            Some(serde_json::to_value(AccountRotationUpdateParams {
                expected_rotation_revision: 0,
                mode: ThreadAccountRotationMode::Fixed,
                fixed_account_slot_id: Some("C2".to_string()),
                automatic_account_slot_ids: Vec::new(),
            })?),
        )
        .await?;
    let stale = app
        .read_stream_until_error_message(RequestId::Integer(stale_id))
        .await?;
    assert_eq!(stale.error.code, -32600);
    assert!(stale.error.message.contains("stale"));

    let read_id = app.send_raw_request("accountRotation/read", None).await?;
    let reread: AccountRotationReadResponse = app.read_response(read_id).await?;
    assert_eq!(reread.rotation, Some(expected));
    Ok(())
}

fn write_owner_catalog(
    owner_home: &std::path::Path,
    accounts: &[(&std::path::Path, u32)],
) -> Result<()> {
    let config = owner_home.join(".config");
    std::fs::create_dir_all(&config)?;
    let catalog = config.join("codex-accounts.tsv");
    let contents = accounts
        .iter()
        .map(|(home, number)| format!("{number}\t{}\n", home.display()))
        .collect::<String>();
    std::fs::write(&catalog, contents)?;
    std::fs::set_permissions(&catalog, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
