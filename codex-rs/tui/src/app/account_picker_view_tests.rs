use super::*;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotSnapshot;
use insta::assert_snapshot;

#[tokio::test]
async fn account_picker_lists_login_and_add_actions() {
    let mut app = make_test_app().await;
    app.account_slots = vec![AccountSlotSnapshot {
        account_slot_id: "default".to_string(),
        label: "Primary".to_string(),
        is_default: true,
        status: AccountSlotStatus::LoginRequired,
        auth_mode: None,
        attempt_generation: 3,
        registry_revision: 7,
        active_login_operation_id: None,
        error_code: None,
        actions: vec![AccountSlotActionAvailability {
            action: AccountSlotAction::Login,
            allowed: true,
            deny_reason: None,
        }],
        updated_at: 0,
    }];
    app.account_slot_capability = Some(AccountSlotCapability {
        available: true,
        deny_reason: None,
    });

    let params = app.account_selection_view_params();
    let rendered = params
        .items
        .iter()
        .map(|item| {
            format!(
                "{} | {} | {}",
                item.name,
                item.description.as_deref().unwrap_or_default(),
                if item.is_disabled {
                    "disabled"
                } else {
                    "enabled"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert_snapshot!(rendered, @r"
    Primary | Login required | enabled
    Add account | Sign in with a browser or device code | enabled
    ");
}
