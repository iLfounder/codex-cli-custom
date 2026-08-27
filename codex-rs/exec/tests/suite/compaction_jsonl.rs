use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

fn configured_exec_command(
    codex_home: &std::path::Path,
    cwd: &std::path::Path,
    server: &wiremock::MockServer,
) -> anyhow::Result<Command> {
    let provider = format!(
        "model_providers.mock_provider={{name=\"Mock provider for test\",base_url=\"{}/v1\",wire_api=\"responses\",supports_websockets=false}}",
        server.uri()
    );
    let mut command = Command::new(codex_utils_cargo_bin::cargo_bin("codex-exec")?);
    command
        .current_dir(cwd)
        .env("CODEX_HOME", codex_home)
        .env("CODEX_SQLITE_HOME", codex_home)
        .env("CODEX_API_KEY", "dummy")
        .arg("--skip-git-repo-check")
        .arg("--json")
        .arg("-c")
        .arg(provider)
        .args([
            "-c",
            "model_provider=\"mock_provider\"",
            "-c",
            "model_auto_compact_token_limit=200000",
            "-c",
            "features.remote_compaction_v2=false",
            "-c",
            "features.enable_request_compression=false",
        ]);
    Ok(command)
}

fn response(id: &str, message_id: &str, text: &str, total_tokens: i64) -> String {
    responses::sse(vec![
        responses::ev_response_created(id),
        responses::ev_assistant_message(message_id, text),
        responses::ev_completed_with_tokens(id, total_tokens),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_started_is_visible_on_a_pipe_before_completion() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    let delayed_compaction = responses::sse_response(response(
        "response-3",
        "message-3",
        "AUTO_SUMMARY",
        /*total_tokens*/ 200,
    ))
    .set_delay(Duration::from_secs(2));
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(response(
                "response-1",
                "message-1",
                "FIRST_REPLY",
                /*total_tokens*/ 70_000,
            )),
            responses::sse_response(response(
                "response-2",
                "message-2",
                "SECOND_REPLY",
                /*total_tokens*/ 330_000,
            )),
            delayed_compaction,
            responses::sse_response(response(
                "response-4",
                "message-4",
                "FINAL_REPLY",
                /*total_tokens*/ 120,
            )),
        ],
    )
    .await;

    let first = configured_exec_command(test.home_path(), test.cwd_path(), &server)?
        .arg("token limit start")
        .output()?;
    assert!(
        first.status.success(),
        "first exec failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = configured_exec_command(test.home_path(), test.cwd_path(), &server)?
        .args(["resume", "--last", "token limit push"])
        .output()?;
    assert!(
        second.status.success(),
        "second exec failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let mut child = configured_exec_command(test.home_path(), test.cwd_path(), &server)?
        .args(["resume", "--last", "post auto follow-up"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout should be piped");
    let mut stdout = BufReader::new(stdout);
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line)?,
            0,
            "exec ended before compaction started"
        );
        let event: Value = serde_json::from_str(line.trim_end())?;
        let is_compaction_start =
            event["type"] == "item.started" && event["item"]["type"] == "context_compaction";
        lines.push(event);
        if is_compaction_start {
            break;
        }
    }
    assert_eq!(
        child.try_wait()?,
        None,
        "item.started must be pipe-visible while the controlled compaction response is delayed"
    );

    let mut remaining_stdout = String::new();
    stdout.read_to_string(&mut remaining_stdout)?;
    lines.extend(
        remaining_stdout
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?,
    );
    let status = child.wait()?;
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr should be piped")
        .read_to_string(&mut stderr)?;
    assert!(status.success(), "compacting exec failed: {stderr}");

    let compaction_events = lines
        .iter()
        .filter(|event| event["item"]["type"] == "context_compaction")
        .collect::<Vec<_>>();
    assert!(
        compaction_events
            .iter()
            .any(|event| event["type"] == "item.updated")
    );
    let terminal = compaction_events
        .iter()
        .find(|event| event["type"] == "item.completed")
        .expect("compaction completion missing");
    assert_eq!(terminal["item"]["status"], "completed");
    assert!(terminal["item"]["completed_at_ms"].is_number());
    assert!(terminal["item"]["duration_ms"].is_number());
    assert_eq!(response_mock.requests().len(), 4);

    Ok(())
}
