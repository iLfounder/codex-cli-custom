//! Ignored controls smoke against a fresh caller-owned Mac fixture, one scenario per run.
//! Select a packaged CLI with CARGO_BIN_EXE_codex and existing numeric-loopback forwards
//! with CODEX_REAL_REMOTE_WS/CODEX_REAL_REMOTE_FIXTURE_HTTP plus CODEX_REAL_REMOTE_CWD.
//! The fixture must use --scenario account-switch or approval, without --hold-first-response.
//! These tests never start SSH, install packages, or access real account credentials.

use super::real::loopback_url;
use super::real::rpc;
use super::*;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

const MODEL: &str = "remote-smoke-model";
const THREAD_NAME: &str = "footer-control-smoke";

#[derive(Deserialize)]
struct ControlStatus {
    scenario: String,
    available_account_slots: Vec<String>,
    account_slots: Vec<Option<String>>,
    observed: bool,
    released: bool,
    response_count: usize,
    completed_response_count: usize,
    phases: Vec<String>,
    turn_ids: Vec<Option<String>>,
}

struct ControlFixture {
    remote: Url,
    status_url: Url,
    cwd: String,
    http: codex_http_client::HttpClient,
    observer: RemoteAppServerClient,
}

impl ControlFixture {
    async fn connect(scenario: &str, slots: &[&str]) -> Result<Self> {
        let remote = loopback_url("CODEX_REAL_REMOTE_WS", "ws")?;
        let fixture = loopback_url("CODEX_REAL_REMOTE_FIXTURE_HTTP", "http")?;
        let cwd =
            std::env::var("CODEX_REAL_REMOTE_CWD").context("missing CODEX_REAL_REMOTE_CWD")?;
        ensure!(
            cwd.starts_with('/') && !cwd.chars().any(char::is_control),
            "expected an absolute Mac cwd"
        );
        ensure!(
            std::env::var_os("CARGO_BIN_EXE_codex").is_some(),
            "select a prebuilt CLI with CARGO_BIN_EXE_codex"
        );
        let observer = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url: remote.to_string(),
                auth_token: None,
            },
            client_name: "windows-remote-controls-smoke".into(),
            client_version: "0.1.0".into(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 16,
        })
        .await
        .map_err(|_| anyhow::anyhow!("could not initialize isolated app-server observer"))?;
        let fixture = Self {
            remote,
            cwd,
            observer,
            status_url: fixture.join("/fixture/first-request")?,
            http: codex_http_client::HttpClientBuilder::new()
                .without_redirects()
                .build_direct()?,
        };
        let initial = fixture.status().await?;
        ensure!(
            initial.scenario == scenario && initial.available_account_slots == slots,
            "wrong isolated fixture scenario or synthetic account catalog"
        );
        ensure!(
            !initial.observed
                && initial.released
                && initial.response_count == 0
                && initial.completed_response_count == 0
                && initial.account_slots.is_empty(),
            "fixture must be fresh and must not hold its first response"
        );
        let listed = rpc(
            &fixture.observer,
            "thread/list",
            json!({"limit": 100, "modelProviders": []}),
        )
        .await?;
        ensure!(
            listed["data"].as_array().is_some_and(Vec::is_empty),
            "Mac app-server owner must have no existing threads"
        );
        Ok(fixture)
    }

    async fn status(&self) -> Result<ControlStatus> {
        let response = self
            .http
            .get(self.status_url.clone())
            .timeout(Duration::from_secs(/*secs*/ 5))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("fixture status request failed"))?;
        ensure!(
            response.status().is_success(),
            "fixture status HTTP failure"
        );
        response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("fixture status schema mismatch"))
    }

    async fn wait_completed(&self, expected: usize, maximum: usize) -> Result<ControlStatus> {
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            let status = self.status().await?;
            ensure!(
                status.response_count <= maximum
                    && status.completed_response_count <= status.response_count,
                "fixture received unexpected model requests"
            );
            if status.completed_response_count == expected {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "fixture did not finish its expected responses"
            );
            tokio::time::sleep(Duration::from_millis(/*millis*/ 25)).await;
        }
    }

    async fn only_thread_id(&self) -> Result<String> {
        let listed = rpc(
            &self.observer,
            "thread/list",
            json!({"limit": 100, "modelProviders": []}),
        )
        .await?;
        let threads = listed["data"]
            .as_array()
            .context("thread list missing data")?;
        ensure!(
            threads.len() == 1 && threads[0]["cwd"] == self.cwd,
            "expected exactly one thread at the isolated Mac cwd"
        );
        Ok(threads[0]["id"]
            .as_str()
            .context("thread ID missing")?
            .to_string())
    }

    async fn wait_runtime(
        &self,
        thread_id: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            // This observer never resumes/subscribes to the thread or changes its settings.
            let page = rpc(
                &self.observer,
                "sessionRuntime/list",
                json!({"threadId": thread_id, "limit": 1}),
            )
            .await?;
            let entries = page["data"]
                .as_array()
                .context("runtime list missing data")?;
            ensure!(
                entries.len() == 1 && entries[0]["threadId"] == thread_id,
                "exact runtime missing"
            );
            if predicate(&entries[0]) {
                return Ok(entries[0].clone());
            }
            ensure!(
                Instant::now() < deadline,
                "runtime did not reach the expected control state"
            );
            tokio::time::sleep(Duration::from_millis(/*millis*/ 25)).await;
        }
    }
}

fn footer_config(config: &mut Value) {
    config["tui"]["footer"] = json!({
        "enabled": true, "max_rows": 3, "border": "rounded",
        "rows": [
            {"left": ["model", "reasoning_effort"], "right": ["account_slot"]},
            {"left": ["thread_name"], "right": ["thread_id"]},
            {"left": ["context_usage"], "right": ["runtime_state"]}
        ],
        "colors": {"model": "cyan", "reasoning_effort": "magenta", "account_slot": "green"}
    });
}

fn text_column(screen: &vt100::Screen, row: u16, text: &str) -> Option<u16> {
    let (_, width) = screen.size();
    let chars: Vec<_> = text.chars().collect();
    (0..width).find(|column| {
        chars.iter().enumerate().all(|(index, expected)| {
            screen
                .cell(row, column.saturating_add(index as u16))
                .is_some_and(|cell| cell.contents() == expected.to_string())
        })
    })
}

fn footer_matches(screen: &vt100::Screen, slot: &str, thread_id: &str) -> bool {
    let (height, _) = screen.size();
    (0..height.saturating_sub(2)).any(|row| {
        let (Some(model), Some(effort), Some(account)) = (
            text_column(screen, row, MODEL),
            text_column(screen, row, "medium"),
            text_column(screen, row, slot),
        ) else {
            return false;
        };
        model < effort
            && effort < account
            && screen
                .cell(row, model)
                .is_some_and(|cell| cell.fgcolor() == vt100::Color::Idx(6))
            && screen
                .cell(row, effort)
                .is_some_and(|cell| cell.fgcolor() == vt100::Color::Idx(5))
            && screen
                .cell(row, account)
                .is_some_and(|cell| cell.fgcolor() == vt100::Color::Idx(2))
            && text_column(screen, row + 1, THREAD_NAME)
                .zip(text_column(screen, row + 1, thread_id))
                .is_some_and(|(name, id)| name < id)
            && text_column(screen, row + 2, "Context ")
                .zip(text_column(screen, row + 2, "idle"))
                .is_some_and(|(context, idle)| context < idle)
            && text_column(screen, row + 2, "% used").is_some()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires fresh Mac --scenario account-switch fixture and caller-owned SSH forwards"]
async fn windows_real_remote_account_switch_updates_execution_and_configured_footer() -> Result<()>
{
    let fixture = ControlFixture::connect("account-switch", &["C1", "C2"]).await?;
    let (_home, mut terminal) =
        start_terminal_with_config(fixture.remote.as_str(), &fixture.cwd, footer_config).await?;
    terminal.wait_for(&[MODEL]).await?;
    terminal
        .write(format!("/rename {THREAD_NAME}\r").as_bytes())
        .await?;
    terminal.wait_for(&[THREAD_NAME]).await?;
    terminal.write(b"isolated account C1 smoke\r").await?;
    terminal
        .wait_for(&["remote smoke account C1 complete"])
        .await?;
    let first = fixture
        .wait_completed(/*expected*/ 1, /*maximum*/ 2)
        .await?;
    ensure!(
        first.account_slots == [Some("C1".to_string())],
        "first request did not use synthetic C1"
    );
    let thread_id = fixture.only_thread_id().await?;
    let before = fixture
        .wait_runtime(&thread_id, |runtime| {
            runtime["account"]["current"]["accountSlotId"] == "C1"
                && runtime["lifecycle"]["activeTurnId"].is_null()
        })
        .await?;
    let generation = before["account"]["current"]["executionGeneration"]
        .as_u64()
        .context("C1 execution generation missing")?;
    // Opening the account picker also requests fresh server-owned slot metadata.
    terminal.write(b"/account\r").await?;
    terminal.wait_for(&["Accounts", "C1", "C2"]).await?;
    terminal.write(b"\x1b").await?;
    terminal
        .wait_for_screen_matching("C1 footer lanes, colors, thread and live usage", |screen| {
            footer_matches(screen, "C1", &thread_id)
        })
        .await?;
    terminal.write(b"/account\r").await?;
    terminal.wait_for(&["Accounts", "C1", "C2"]).await?;
    // The picker selects current C1. Its next sorted account is the nondefault C2.
    terminal.write(b"\x1b[B\r").await?;
    terminal.wait_for(&["C2", "Use for this session"]).await?;
    terminal.write(b"\r").await?;
    terminal
        .wait_for_screen(&["Accounts", "C2"], &["Use for this session"])
        .await?;
    let after = fixture
        .wait_runtime(&thread_id, |runtime| {
            runtime["account"]["current"]["accountSlotId"] == "C2"
                && runtime["account"]["switchState"] == "stable"
        })
        .await?;
    ensure!(
        after["account"]["current"]["executionGeneration"]
            .as_u64()
            .is_some_and(|value| value > generation),
        "account switch did not advance execution generation"
    );
    terminal.write(b"\x1b").await?;
    terminal
        .wait_for_screen_matching("C2 footer without stale C1 projection", |screen| {
            footer_matches(screen, "C2", &thread_id)
        })
        .await?;
    terminal.write(b"isolated account C2 smoke\r").await?;
    terminal
        .wait_for(&["remote smoke account C2 complete"])
        .await?;
    let completed = fixture
        .wait_completed(/*expected*/ 2, /*maximum*/ 2)
        .await?;
    ensure!(
        completed.response_count == 2
            && completed.account_slots == [Some("C1".into()), Some("C2".into())]
            && completed.phases == ["account_c1_complete", "account_c2_complete"],
        "actual model requests did not follow C1 then C2"
    );
    ensure!(
        completed.turn_ids.len() == 2
            && completed
                .turn_ids
                .iter()
                .all(|id| id.as_ref().is_some_and(|id| !id.is_empty()))
            && completed.turn_ids[0] != completed.turn_ids[1],
        "expected exactly two separate user turns"
    );
    terminal
        .wait_for_screen_matching("idle C2 footer after the second turn", |screen| {
            footer_matches(screen, "C2", &thread_id)
        })
        .await?;
    drop(terminal);

    // Reopen the same server thread with legacy adapter selection, not declarative rows.
    let (_legacy_home, mut legacy) =
        start_terminal_with_config(fixture.remote.as_str(), &fixture.cwd, |config| {
            config["tui"]["footer"] = json!({"enabled": true, "adapter_ids": ["thread"]});
        })
        .await?;
    legacy.wait_for(&[MODEL]).await?;
    legacy
        .write(format!("/resume {thread_id}\r").as_bytes())
        .await?;
    let thread_label = format!("Thread {THREAD_NAME}");
    let id_label = format!("id {thread_id}");
    let legacy_footer_matches = |screen: &vt100::Screen| {
        let (height, width) = screen.size();
        (0..height).any(|row| {
            let Some(name_column) = text_column(screen, row, &thread_label) else {
                return false;
            };
            if width == 48 {
                text_column(screen, row, "…").is_some_and(|column| column > name_column)
                    && text_column(screen, row, &id_label).is_none()
            } else {
                text_column(screen, row, &id_label).is_some_and(|column| column > name_column)
                    && text_column(screen, row, "…").is_none()
            }
        })
    };
    legacy
        .wait_for_screen_matching(
            "legacy thread adapter with full server identity",
            |screen| legacy_footer_matches(screen),
        )
        .await?;
    let mut draft = "legacy-resize-draft".to_string();
    legacy.write(draft.as_bytes()).await?;
    legacy.wait_for(&[&draft]).await?;
    for (rows, cols, suffix) in [(24, 48, "-narrow"), (40, 140, "-wide")] {
        legacy.process.session.resize(TerminalSize { rows, cols })?;
        legacy.parser.screen_mut().set_size(rows, cols);
        // A new unsent suffix requires a fresh frame, not a stale parser screen.
        legacy.write(suffix.as_bytes()).await?;
        draft.push_str(suffix);
        legacy
            .wait_for_screen_matching(
                "resized legacy footer and preserved unsent draft",
                |screen| {
                    screen.size() == (rows, cols)
                        && screen.contents().contains(&draft)
                        && legacy_footer_matches(screen)
                },
            )
            .await?;
    }
    let final_status = fixture.status().await?;
    ensure!(
        final_status.response_count == 2
            && final_status.completed_response_count == 2
            && final_status.phases == completed.phases
            && final_status.account_slots == completed.account_slots
            && final_status.turn_ids == completed.turn_ids,
        "legacy resume or resizing issued an unexpected model request"
    );
    drop(legacy);
    fixture
        .observer
        .shutdown()
        .await
        .map_err(|_| anyhow::anyhow!("observer shutdown failed"))?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires fresh Mac --scenario approval fixture and caller-owned SSH forwards"]
async fn windows_real_remote_approval_blocks_mac_execution_until_one_time_accept() -> Result<()> {
    let fixture = ControlFixture::connect("approval", &["C1"]).await?;
    let (_home, mut terminal) = start_terminal(fixture.remote.as_str(), &fixture.cwd).await?;
    terminal.wait_for(&[MODEL, &fixture.cwd]).await?;
    let objective = format!("Complete isolated approval smoke {}", uuid::Uuid::new_v4());
    terminal
        .write(format!("/goal {objective}\r").as_bytes())
        .await?;
    terminal
        .wait_for(&["Isolated approval smoke", "Yes, proceed"])
        .await?;
    let thread_id = fixture.only_thread_id().await?;
    let waiting = fixture
        .wait_runtime(&thread_id, |runtime| {
            runtime["lifecycle"]["waitingOn"]
                .as_array()
                .is_some_and(|waiting| waiting.iter().any(|item| item == "approval"))
        })
        .await?;
    let settings = &waiting["identity"]["settings"];
    ensure!(
        settings["approvalPolicy"] == "on-request"
            && settings["approvalsReviewer"] == "user"
            && settings["sandboxPolicy"]["type"] == "readOnly",
        "Mac approval policy differs from the isolated fixture"
    );
    let active = rpc(
        &fixture.observer,
        "thread/goal/get",
        json!({"threadId": thread_id}),
    )
    .await?;
    ensure!(
        active["goal"]["status"] == "active" && active["goal"]["objective"] == objective,
        "approval must belong to the TUI-created active goal"
    );
    let paused = fixture
        .wait_completed(/*expected*/ 1, /*maximum*/ 1)
        .await?;
    ensure!(paused.phases == ["pwd"], "unexpected phase before approval");
    // Check that a visible approval actually blocks progress, rather than being cosmetic.
    let pause_deadline = Instant::now() + Duration::from_millis(/*millis*/ 500);
    while Instant::now() < pause_deadline {
        let paused = fixture.status().await?;
        ensure!(
            paused.response_count == 1 && paused.completed_response_count == 1,
            "Mac execution advanced before Windows approval"
        );
        tokio::time::sleep(Duration::from_millis(/*millis*/ 100)).await;
    }
    // Fresh local config uses the standard single-action approve shortcut, not session approval.
    terminal.write(b"y").await?;
    terminal.wait_for(&["remote smoke goal complete"]).await?;
    let completed = fixture
        .wait_completed(/*expected*/ 5, /*maximum*/ 5)
        .await?;
    ensure!(
        completed.response_count == 5
            && completed.phases
                == [
                    "pwd",
                    "first_turn_complete",
                    "get_goal",
                    "update_goal",
                    "goal_complete"
                ],
        "one-time approval did not unblock the expected five-phase goal"
    );
    fixture
        .wait_runtime(&thread_id, |runtime| {
            runtime["lifecycle"]["activeTurnId"].is_null()
        })
        .await?;
    let goal = rpc(
        &fixture.observer,
        "thread/goal/get",
        json!({"threadId": thread_id}),
    )
    .await?;
    ensure!(
        goal["goal"]["status"] == "complete"
            && goal["goal"]["objective"] == objective
            && goal["goal"]["goalId"] == active["goal"]["goalId"],
        "original goal did not complete"
    );
    let history = rpc(
        &fixture.observer,
        "thread/turns/list",
        json!({
            "threadId": thread_id, "limit": 10, "itemsView": "full"
        }),
    )
    .await?;
    let turns = history["data"].as_array().context("turn history missing")?;
    ensure!(
        turns.len() == 2 && turns.iter().all(|turn| turn["status"] == "completed"),
        "expected the initial turn and one completed goal continuation"
    );
    ensure!(
        turns
            .iter()
            .filter_map(|turn| turn["items"].as_array())
            .flatten()
            .any(|item| {
                item["type"] == "commandExecution"
                    && item["cwd"] == fixture.cwd
                    && item["exitCode"] == 0
                    && item["aggregatedOutput"]
                        .as_str()
                        .is_some_and(|output| output.lines().any(|line| line.trim() == fixture.cwd))
            }),
        "approved pwd did not execute in the actual Mac cwd"
    );
    drop(terminal);
    fixture
        .observer
        .shutdown()
        .await
        .map_err(|_| anyhow::anyhow!("observer shutdown failed"))?;
    Ok(())
}
