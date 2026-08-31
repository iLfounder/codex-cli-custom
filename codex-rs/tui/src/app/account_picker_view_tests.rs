use super::*;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotSnapshot;
use insta::assert_snapshot;

#[tokio::test]
async fn account_picker_enables_ready_target_but_not_current_slot() {
    let mut app = make_test_app().await;
    app.account_slots = vec![
        AccountSlotSnapshot {
            account_slot_id: "default".to_string(),
            label: "Primary".to_string(),
            is_default: true,
            status: AccountSlotStatus::Ready,
            auth_mode: None,
            attempt_generation: 3,
            registry_revision: 7,
            active_login_operation_id: None,
            error_code: None,
            actions: vec![AccountSlotActionAvailability {
                action: AccountSlotAction::SwitchTo,
                allowed: true,
                deny_reason: None,
            }],
            updated_at: 0,
        },
        AccountSlotSnapshot {
            account_slot_id: "secondary".to_string(),
            label: "Secondary".to_string(),
            is_default: false,
            status: AccountSlotStatus::Ready,
            auth_mode: None,
            attempt_generation: 2,
            registry_revision: 7,
            active_login_operation_id: None,
            error_code: None,
            actions: vec![
                AccountSlotActionAvailability {
                    action: AccountSlotAction::SwitchTo,
                    allowed: true,
                    deny_reason: None,
                },
                AccountSlotActionAvailability {
                    action: AccountSlotAction::Logout,
                    allowed: true,
                    deny_reason: None,
                },
                AccountSlotActionAvailability {
                    action: AccountSlotAction::RetryLogin,
                    allowed: true,
                    deny_reason: None,
                },
            ],
            updated_at: 0,
        },
    ];
    app.account_slot_capability = Some(AccountSlotCapability {
        available: true,
        deny_reason: None,
    });
    app.account_runtime = Some((
        "instance".to_string(),
        serde_json::from_value(serde_json::json!({
            "threadId": "thread",
            "stateRevision": 1,
            "identity": {
                "sessionId": "thread",
                "forkedFromId": null,
                "parentThreadId": null,
                "name": null,
                "source": "cli",
                "cwd": "/tmp",
                "gitInfo": null,
                "settings": null
            },
            "lifecycle": {
                "state": "idle",
                "activeTurnId": null,
                "waitingOn": [],
                "subscriberCount": 1,
                "clientIncarnations": [],
                "lastActivityAt": null,
                "unloadAt": null
            },
            "writer": {
                "state": "ownedHere",
                "storeId": null,
                "writerGeneration": 1,
                "denyReason": null
            },
            "persistence": {
                "jsonl": null,
                "sqlite": null,
                "lag": null,
                "flushHealth": "unknown",
                "materializeHealth": "unknown",
                "flushedAt": null,
                "materializedAt": null,
                "denyReason": null
            },
            "account": {
                "current": {
                    "accountSlotId": "default",
                    "executionGeneration": 1
                },
                "activeTurn": null,
                "switchState": "stable",
                "switchTargetSlotId": null,
                "denyReason": null
            },
            "actions": [{
                "action": "switchAccount",
                "allowed": true,
                "denyReason": null
            }]
        }))
        .expect("runtime snapshot"),
    ));

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
    Primary | Ready | disabled
    Secondary | Ready | enabled
    Sign in again to Secondary | Replace credentials for every idle bound session | enabled
    Log out Secondary | Remove this secondary account | enabled
    Add account | Sign in with a browser or device code | enabled
    ");
}
