use super::*;
use crate::app::account_login::login_challenge_params;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotLoginChallenge;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
fn slot(
    id: &str,
    status: AccountSlotStatus,
    active_login: Option<&str>,
    actions: &[(AccountSlotAction, bool, Option<&str>)],
) -> AccountSlotSnapshot {
    AccountSlotSnapshot {
        account_slot_id: id.to_string(),
        account_number: match id {
            "default" => 1,
            "secondary" | "failed" => 2,
            "active" => 3,
            _ => 4,
        },
        label: id.to_string(),
        is_default: id == "default",
        status,
        auth_mode: None,
        attempt_generation: 1,
        registry_revision: 7,
        active_login_operation_id: active_login.map(str::to_string),
        error_code: Some("oauth_failed".to_string()),
        actions: actions
            .iter()
            .map(|(action, allowed, reason)| AccountSlotActionAvailability {
                action: *action,
                allowed: *allowed,
                deny_reason: reason.map(str::to_string),
            })
            .collect(),
        updated_at: 0,
    }
}
fn rendered_items(params: &SelectionViewParams) -> String {
    params
        .items
        .iter()
        .map(|item| {
            format!(
                "{} | {} | {}{}",
                item.name,
                item.description.as_deref().unwrap_or_default(),
                if item.is_disabled {
                    "disabled"
                } else {
                    "enabled"
                },
                item.disabled_reason
                    .as_ref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn login_challenge_requires_explicit_browser_or_cancel_selection() {
    let params = login_challenge_params(
        "secondary".to_string(),
        AccountSlotLoginChallenge::DeviceCode {
            login_id: "login-1".to_string(),
            verification_url: "https://example.test/device".to_string(),
            user_code: "ABCD-EFGH".to_string(),
        },
    );
    assert_snapshot!(format!("{}\n{}", params.subtitle.unwrap(), params.items.iter().map(|item| item.name.as_str()).collect::<Vec<_>>().join(" | ")), @r"
    Open https://example.test/device
    and enter code ABCD-EFGH
    Open Browser | Cancel login
    ");
}
#[tokio::test]
async fn account_rows_are_always_selectable_and_report_active_login() {
    let mut app = make_test_app().await;
    app.account_slots = vec![
        slot("default", AccountSlotStatus::Ready, None, &[]),
        slot("failed", AccountSlotStatus::Failed, None, &[]),
        slot(
            "active",
            AccountSlotStatus::LoginRequired,
            Some("login-1"),
            &[],
        ),
    ];
    assert_snapshot!(rendered_items(&app.account_selection_view_params(None)), @r"
    1. default | Ready · Error: oauth_failed | enabled
    2. failed | Login failed · Error: oauth_failed | enabled
    3. active | Login required · Login in progress · Error: oauth_failed | enabled
    Add account | Sign in with a browser or device code | disabled
    ");
}
#[tokio::test]
async fn account_detail_disables_only_unavailable_actions() {
    let app = make_test_app().await;
    let ready = slot(
        "secondary",
        AccountSlotStatus::Ready,
        None,
        &[
            (AccountSlotAction::RetryLogin, false, Some("policy")),
            (AccountSlotAction::SwitchTo, true, None),
            (AccountSlotAction::Logout, true, None),
        ],
    );
    assert_snapshot!(rendered_items(&app.account_detail_view_params(&ready)), @r"
    Log in | Authenticate this account | disabled (Account does not require login)
    Retry login | Retry the failed sign-in | disabled (No failed login to retry)
    Sign in again | Replace this account's credentials | disabled (policy)
    Cancel login | Stop the active sign-in attempt | disabled (No login is in progress)
    Use for this session | Switch the next turn to this account | disabled (Account switching is unavailable)
    Log out | Remove this account's credentials | enabled
    ");
    let ready_params = app.account_detail_view_params(&ready);
    assert_eq!(
        ready_params.subtitle.as_deref(),
        Some("Ready · Error: oauth_failed · Secondary account")
    );

    let failed = slot(
        "secondary",
        AccountSlotStatus::Failed,
        None,
        &[
            (AccountSlotAction::RetryLogin, true, None),
            (
                AccountSlotAction::SwitchTo,
                false,
                Some("Account is not ready"),
            ),
            (
                AccountSlotAction::Logout,
                false,
                Some("Account is not ready"),
            ),
        ],
    );
    assert_snapshot!(rendered_items(&app.account_detail_view_params(&failed)), @r"
    Log in | Authenticate this account | disabled (Account does not require login)
    Retry login | Retry the failed sign-in | enabled
    Sign in again | Replace this account's credentials | disabled (Account is not ready)
    Cancel login | Stop the active sign-in attempt | disabled (No login is in progress)
    Use for this session | Switch the next turn to this account | disabled (Account is not ready)
    Log out | Remove this account's credentials | disabled (Account is not ready)
    ");

    let active = slot(
        "secondary",
        AccountSlotStatus::LoginRequired,
        Some("login-1"),
        &[
            (
                AccountSlotAction::Login,
                false,
                Some("Login already in progress"),
            ),
            (
                AccountSlotAction::RetryLogin,
                false,
                Some("Login already in progress"),
            ),
            (
                AccountSlotAction::SwitchTo,
                false,
                Some("Login already in progress"),
            ),
            (
                AccountSlotAction::Logout,
                false,
                Some("Login already in progress"),
            ),
        ],
    );
    assert_snapshot!(rendered_items(&app.account_detail_view_params(&active)), @r"
    Log in | Authenticate this account | disabled (Login already in progress)
    Retry login | Retry the failed sign-in | disabled (No failed login to retry)
    Sign in again | Replace this account's credentials | disabled (Account is not ready)
    Cancel login | Stop the active sign-in attempt | enabled
    Use for this session | Switch the next turn to this account | disabled (Login already in progress)
    Log out | Remove this account's credentials | disabled (Login already in progress)
    ");
}

#[tokio::test]
async fn selection_is_preserved_by_account_slot_identity() {
    let mut app = make_test_app().await;
    app.account_slots = vec![
        slot("default", AccountSlotStatus::Ready, None, &[]),
        slot("secondary", AccountSlotStatus::Ready, None, &[]),
    ];
    assert_eq!(
        app.account_selection_view_params(Some("secondary"))
            .initial_selected_idx,
        Some(1)
    );
}

#[tokio::test]
async fn replacing_a_closed_picker_updates_no_ui() {
    let mut app = make_test_app().await;
    app.account_slots = vec![slot("default", AccountSlotStatus::Ready, None, &[])];
    assert_eq!(app.replace_account_picker_if_present(None), false);
    assert_eq!(app.chat_widget.no_modal_or_popup_active(), true);
}

#[test]
fn exact_status_and_error_code_are_both_visible() {
    let mut unavailable = slot("secondary", AccountSlotStatus::Failed, None, &[]);
    unavailable.error_code = Some("authUnavailable".to_string());
    assert_eq!(
        account_slot_status_label(&unavailable),
        "Login failed · Error: authUnavailable"
    );
}
