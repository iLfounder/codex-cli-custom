//! Real Windows CLI/ConPTY coverage for remote authority and transport recovery.
//! `CARGO_BIN_EXE_codex` can select an already-built release without rebuilding it.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartResponse;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

const SERVER_CWD: &str = "/server/project";
const SERVER_MODEL: &str = "server-model";
const SERVER_PROVIDER: &str = "server-provider";
const SCREEN_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 30);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_remote_authority_and_reconnect_preserve_draft_without_replaying_turn() -> Result<()>
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let (restore_tx, restore_rx) = oneshot::channel();
    let mut server = OwnedServer(tokio::spawn(async move {
        // Includes both connections and the deliberately held recovery handshake.
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 150),
            serve_remote(listener, disconnect_rx, restore_rx),
        )
        .await
        .context("mock server exceeded the smoke deadline")?
    }));
    let (_home, mut terminal) = start_terminal(&endpoint, SERVER_CWD).await?;
    terminal.wait_for(&[SERVER_MODEL, SERVER_CWD]).await?;
    terminal.write(b"/status\r").await?;
    terminal
        .wait_for(&["Model provider", SERVER_PROVIDER, SERVER_CWD])
        .await?;
    let status = terminal.parser.screen().contents();
    for local_only in [
        "windows-only-model",
        "windows-only-provider",
        "windows-client-project",
    ] {
        ensure!(
            !status.contains(local_only),
            "local authority appeared in remote status: {status}"
        );
    }

    terminal.write(b"accepted-once\r").await?;
    terminal.wait_for(&["server-accepted-marker"]).await?;
    terminal.write(b"preserved-draft").await?;
    terminal.wait_for(&["preserved-draft"]).await?;
    disconnect_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("mock server exited before disconnect"))?;
    terminal.wait_for(&["Reconnecting"]).await?;
    terminal.write(b"-offline").await?;
    terminal.wait_for(&["preserved-draft-offline"]).await?;
    restore_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("mock server exited before restore"))?;
    terminal
        .wait_for(&[
            "fresh-after-reconnect",
            "preserved-draft-offline",
            SERVER_MODEL,
        ])
        .await?;

    // ProcessHandle drop terminates only this ConPTY child. Closing it also lets
    // the mock finish collecting requests, including any unintended replay.
    drop(terminal);
    let requests = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), &mut server.0).await???;
    let starts: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "thread/start")
        .collect();
    let resumes: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "thread/resume")
        .collect();
    let turns: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "turn/start")
        .collect();
    assert_eq!((starts.len(), resumes.len(), turns.len()), (1, 1, 1));
    assert_eq!(starts[0].params.as_ref().unwrap()["cwd"], SERVER_CWD);
    assert_eq!(
        turns[0].params.as_ref().unwrap()["input"][0]["text"],
        "accepted-once"
    );
    for request in starts.into_iter().chain(resumes).chain(turns) {
        let params = request.params.as_ref().unwrap();
        let encoded = params.to_string();
        for local_only in [
            "windows-only-model",
            "windows-only-provider",
            "windows-client-project",
        ] {
            ensure!(
                !encoded.contains(local_only),
                "{} leaked local authority: {params}",
                request.method
            );
        }
        assert!(
            params["runtimeWorkspaceRoots"].is_null(),
            "{} sent local roots",
            request.method
        );
        if request.method != "turn/start" {
            assert!(params["model"].is_null());
            assert!(params["modelProvider"].is_null());
        }
    }
    Ok(())
}

async fn start_terminal(endpoint: &str, server_cwd: &str) -> Result<(tempfile::TempDir, Terminal)> {
    start_terminal_with_config(endpoint, server_cwd, |_| {}).await
}

async fn start_terminal_with_config(
    endpoint: &str,
    server_cwd: &str,
    configure: impl FnOnce(&mut serde_json::Value),
) -> Result<(tempfile::TempDir, Terminal)> {
    let home = tempfile::tempdir()?;
    let local_cwd = home.path().join("windows-client-project");
    let codex_home = home.path().join("codex-home");
    let sqlite_home = home.path().join("sqlite");
    for directory in [&local_cwd, &codex_home, &sqlite_home] {
        std::fs::create_dir(directory)?;
    }
    let mut config = json!({
        "model": "windows-only-model",
        "model_provider": "windows-only-provider",
        "suppress_unstable_features_warning": true,
        "analytics": {"enabled": false},
        "check_for_update_on_startup": false,
        "model_providers": {"windows-only-provider": {
            "name": "Windows-only provider", "base_url": "http://127.0.0.1:1/v1",
            "wire_api": "responses", "requires_openai_auth": false
        }},
        "tui": {
            // Commands arrive in one ConPTY write, not at human typing intervals.
            "disable_paste_burst": true,
            "status_line": ["model-name", "current-dir"],
            "keymap": {"global": {"open_agents": "alt-a"}}
        },
        "projects": {}
    });
    configure(&mut config);
    config["projects"][local_cwd.to_string_lossy().as_ref()] = json!({"trust_level": "trusted"});
    std::fs::write(codex_home.join("config.toml"), toml::to_string(&config)?)?;

    // Do not inherit account, auth, provider, or user startup configuration.
    let mut env: HashMap<String, String> = std::env::vars()
        .filter(|(key, _)| {
            matches!(
                key.to_ascii_uppercase().as_str(),
                "SYSTEMROOT" | "WINDIR" | "PATH" | "PATHEXT" | "COMSPEC" | "TEMP" | "TMP"
            )
        })
        .collect();
    for (name, path) in [
        ("HOME", home.path()),
        ("USERPROFILE", home.path()),
        ("CODEX_TEST_OWNER_HOME", home.path()),
        ("CODEX_HOME", codex_home.as_path()),
        ("CODEX_SQLITE_HOME", sqlite_home.as_path()),
    ] {
        env.insert(name.to_string(), path.to_string_lossy().into_owned());
    }
    env.insert("TERM".into(), "xterm-256color".into());
    let binary = codex_utils_cargo_bin::cargo_bin("codex")?;
    let process = codex_utils_pty::spawn_pty_process(
        binary.to_str().context("CLI path must be UTF-8")?,
        &[
            "--no-alt-screen".into(),
            "--remote".into(),
            endpoint.to_string(),
            "-C".into(),
            server_cwd.to_string(),
        ],
        &local_cwd,
        &env,
        &None,
        TerminalSize {
            rows: 40,
            cols: 140,
        },
        &[],
    )
    .await?;
    let terminal = Terminal {
        process,
        parser: vt100::Parser::new(
            /*rows*/ 40, /*cols*/ 140, /*scrollback_len*/ 0,
        ),
        query_bytes: Vec::new(),
        answered: [false; 3],
    };
    Ok((home, terminal))
}

#[path = "windows_remote_real_tests.rs"]
mod real;

#[path = "windows_remote_control_tests.rs"]
mod control;

struct OwnedServer(JoinHandle<Result<Vec<JSONRPCRequest>>>);

impl Drop for OwnedServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve_remote(
    listener: TcpListener,
    mut disconnect: oneshot::Receiver<()>,
    restore: oneshot::Receiver<()>,
) -> Result<Vec<JSONRPCRequest>> {
    let id = uuid::Uuid::new_v4().to_string();
    let turn = json!({"id": "accepted-turn", "items": [], "status": "inProgress", "error": null});
    let mut thread = json!({
        "id": id, "sessionId": id, "preview": "", "ephemeral": false,
        "modelProvider": SERVER_PROVIDER, "model": SERVER_MODEL, "reasoningEffort": "medium",
        "createdAt": 1, "updatedAt": 2, "status": {"type": "idle"},
        "cwd": SERVER_CWD, "cliVersion": "0.153.3", "source": "cli", "turns": []
    });
    let models = json!({"data": [{
        "id": SERVER_MODEL, "model": SERVER_MODEL, "displayName": SERVER_MODEL,
        "description": "Remote fixture model", "hidden": false, "isDefault": true,
        "supportedReasoningEfforts": [{"reasoningEffort": "medium", "description": "Medium"}],
        "defaultReasoningEffort": "medium", "inputModalities": ["text"]
    }], "nextCursor": null});
    let _: ModelListResponse = serde_json::from_value(models.clone())?;
    let config = json!({
        "config": {
            "model": SERVER_MODEL,
            "model_provider": SERVER_PROVIDER,
            "projects": {(SERVER_CWD): {"trust_level": "trusted"}}
        },
        "origins": {},
        "layers": []
    });
    let _: ConfigReadResponse = serde_json::from_value(config.clone())?;
    let mut requests = Vec::new();
    let mut restore = Some(restore);
    for connection in 0..2 {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        loop {
            let frame = tokio::select! {
                _ = &mut disconnect, if connection == 0 => break,
                frame = socket.next() => frame,
            };
            // Windows child termination may reset TCP rather than send a Close frame.
            let Some(Ok(frame)) = frame else { break };
            let Message::Text(text) = frame else { continue };
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            if connection == 1 && request.method == "initialize" {
                restore.take().unwrap().await?;
            }
            let result = match request.method.as_str() {
                "initialize" => json!({
                    "userAgent": "windows-remote-smoke/0.153.3", "codexHome": "/server/codex-home",
                    "platformFamily": "unix", "platformOs": "macos"
                }),
                "account/read" => json!({"account": null, "requiresOpenaiAuth": false}),
                "model/list" => models.clone(),
                "config/read" => config.clone(),
                "configRequirements/read" => json!({"requirements": null}),
                "thread/start" | "thread/resume" => {
                    let result = json!({
                        "thread": thread, "model": SERVER_MODEL, "modelProvider": SERVER_PROVIDER,
                        "cwd": SERVER_CWD, "runtimeWorkspaceRoots": [SERVER_CWD],
                        "approvalPolicy": "never", "approvalsReviewer": "user",
                        "sandbox": {"type": "readOnly"}, "reasoningEffort": "medium"
                    });
                    if request.method == "thread/start" {
                        let _: ThreadStartResponse = serde_json::from_value(result.clone())?;
                    } else {
                        let _: ThreadResumeResponse = serde_json::from_value(result.clone())?;
                    }
                    result
                }
                "thread/read" => json!({"thread": thread}),
                "thread/goal/get" => json!({"goal": null, "revision": 0}),
                "skills/list" => json!({"data": []}),
                "turn/start" => {
                    thread["turns"] = json!([turn]);
                    thread["status"] = json!({"type": "active", "activeFlags": []});
                    json!({"turn": turn})
                }
                _ => {
                    socket
                        .send(Message::Text(
                            json!({"id": request.id, "error": {
                                "code": -32601, "message": "unsupported smoke request"
                            }})
                            .to_string()
                            .into(),
                        ))
                        .await?;
                    requests.push(request);
                    continue;
                }
            };
            socket
                .send(Message::Text(
                    json!({"id": request.id, "result": result})
                        .to_string()
                        .into(),
                ))
                .await?;
            let delta = match (connection, request.method.as_str()) {
                (0, "turn/start") => {
                    socket
                        .send(Message::Text(
                            json!({"method": "turn/started", "params": {
                                "threadId": id, "turn": turn
                            }})
                            .to_string()
                            .into(),
                        ))
                        .await?;
                    Some("server-accepted-marker\n")
                }
                (1, "thread/resume") => Some("fresh-after-reconnect\n"),
                _ => None,
            };
            if let Some(delta) = delta {
                socket.send(Message::Text(json!({"method": "item/agentMessage/delta", "params": {
                    "threadId": id, "turnId": "accepted-turn", "itemId": "live-item", "delta": delta
                }}).to_string().into())).await?;
            }
            requests.push(request);
        }
    }
    Ok(requests)
}

struct Terminal {
    process: SpawnedProcess,
    parser: vt100::Parser,
    query_bytes: Vec<u8>,
    answered: [bool; 3],
}

impl Terminal {
    async fn write(&self, bytes: &[u8]) -> Result<()> {
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 5),
            self.process.session.writer_sender().send(bytes.to_vec()),
        )
        .await??;
        Ok(())
    }

    async fn wait_for(&mut self, expected: &[&str]) -> Result<()> {
        self.wait_for_screen(expected, &[]).await
    }

    async fn wait_for_screen(&mut self, expected: &[&str], absent: &[&str]) -> Result<()> {
        self.wait_for_screen_matching(&format!("{expected:?}; absent {absent:?}"), |screen| {
            let screen = screen.contents();
            expected.iter().all(|text| screen.contains(text))
                && absent.iter().all(|text| !screen.contains(text))
        })
        .await
    }

    async fn wait_for_screen_matching(
        &mut self,
        description: &str,
        matches: impl Fn(&vt100::Screen) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            let screen = self.parser.screen().contents();
            if matches(self.parser.screen()) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "missing {description}; screen:\n{screen}"
            );
            let chunk = tokio::time::timeout_at(deadline, self.process.stdout_rx.recv())
                .await
                .with_context(|| format!("missing {description}; screen:\n{screen}"))?
                .with_context(|| format!("ConPTY child closed; screen:\n{screen}"))?;
            self.parser.process(&chunk);
            self.query_bytes.extend_from_slice(&chunk);
            // Same cursor/keyboard/palette replies as focus_palette's Unix PTY.
            let queries: [(&[u8], &[u8]); 3] = [
                (b"\x1b[6n", b"\x1b[1;1R"),
                (b"\x1b[?u", b"\x1b[?0u\x1b[?1;2c"),
                (
                    b"\x1b]11;?",
                    b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\",
                ),
            ];
            for (index, (query, reply)) in queries.iter().enumerate() {
                if !self.answered[index]
                    && self
                        .query_bytes
                        .windows(query.len())
                        .any(|window| window == *query)
                {
                    self.write(reply).await?;
                    self.answered[index] = true;
                }
            }
            if self.query_bytes.len() > 8192 {
                self.query_bytes.drain(..self.query_bytes.len() - 8192);
            }
        }
    }
}
