#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use predicates::str::contains;
use serde_json::Value;
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::time::Duration;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_appends_piped_stdin_to_prompt_argument() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    // echo "my output" | codex exec --skip-git-repo-check -C <cwd> -m gpt-5.1 "Summarize this concisely"
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(test.cwd_path())
        .arg("-m")
        .arg("gpt-5.1")
        .arg("Summarize this concisely")
        .write_stdin("my output\n")
        .assert()
        .success();

    let request = response_mock.single_request();
    assert!(
        request.has_message_with_input_texts("user", |texts| {
            texts == ["Summarize this concisely\n\n<stdin>\nmy output\n</stdin>".to_string()]
        }),
        "request should include a user message with the prompt plus piped stdin context"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_ignores_empty_piped_stdin_when_prompt_argument_is_present() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    // printf "" | codex exec --skip-git-repo-check -C <cwd> -m gpt-5.1 "Summarize this concisely"
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(test.cwd_path())
        .arg("-m")
        .arg("gpt-5.1")
        .arg("Summarize this concisely")
        .write_stdin("")
        .assert()
        .success();

    let request = response_mock.single_request();
    assert!(
        request.has_message_with_input_texts("user", |texts| texts
            == ["Summarize this concisely".to_string()]),
        "request should preserve the prompt when stdin is empty"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_dash_prompt_reads_stdin_as_the_prompt() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    // echo "prompt from stdin" | codex exec --skip-git-repo-check -C <cwd> -m gpt-5.1 -
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(test.cwd_path())
        .arg("-m")
        .arg("gpt-5.1")
        .arg("-")
        .write_stdin("prompt from stdin\n")
        .assert()
        .success();

    let request = response_mock.single_request();
    assert!(
        request.has_message_with_input_texts("user", |texts| {
            texts == ["prompt from stdin\n".to_string()]
        }),
        "dash prompt should preserve the existing forced-stdin behavior"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_without_prompt_argument_reads_piped_stdin_as_the_prompt() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    let response_mock = responses::mount_sse_once(&server, body).await;

    // echo "prompt from stdin" | codex exec --skip-git-repo-check -C <cwd> -m gpt-5.1
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(test.cwd_path())
        .arg("-m")
        .arg("gpt-5.1")
        .write_stdin("prompt from stdin\n")
        .assert()
        .success();

    let request = response_mock.single_request();
    assert!(
        request.has_message_with_input_texts("user", |texts| {
            texts == ["prompt from stdin\n".to_string()]
        }),
        "missing prompt argument should preserve the existing piped-stdin prompt behavior"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invocation_ready_is_flushed_before_new_prompt_read() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-ready-new"),
        responses::ev_assistant_message("msg-ready-new", "fixture hello"),
        responses::ev_completed("resp-ready-new"),
    ]);
    let _response_mock = responses::mount_sse_once(&server, body).await;

    let mut command = test.cmd_with_server(&server);
    command
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(test.cwd_path())
        .arg("--json")
        .arg("--account-failover=pre-semantic")
        .arg("--invocation-ready-id")
        .arg("new-ready-01")
        .arg("-");
    let mut child_command = Command::new(command.get_program());
    child_command
        .args(command.get_args())
        .envs(
            command
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .current_dir(test.cwd_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child_command.spawn()?;
    let mut stdin = child.stdin.take().expect("stdin should be piped");
    let stdout = child.stdout.take().expect("stdout should be piped");
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    timeout(Duration::from_secs(30), stdout.read_line(&mut line)).await??;
    let ready: Value = serde_json::from_str(&line)?;
    assert_eq!(ready["type"], "invocation.ready");
    assert_eq!(ready["invocation_id"], "new-ready-01");
    assert_eq!(ready["account_failover"], "pre_semantic");
    assert_eq!(ready["account_rotation"]["requested"], Value::Null);

    let mut later_line = String::new();
    assert!(
        timeout(
            Duration::from_millis(200),
            stdout.read_line(&mut later_line)
        )
        .await
        .is_err()
    );

    stdin.write_all(b"prompt after readiness\n").await?;
    stdin.shutdown().await?;
    if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
        child.kill().await?;
        child.wait().await?;
    }
    Ok(())
}

#[test]
fn invocation_ready_rejects_invalid_flag_combinations() {
    let test = test_codex_exec();

    test.cmd()
        .arg("--invocation-ready-id")
        .arg("ready-without-json")
        .arg("-")
        .assert()
        .failure()
        .stderr(contains("--invocation-ready-id requires --json"));

    test.cmd()
        .arg("--json")
        .arg("--account-failover=pre-semantic")
        .arg("--invocation-ready-id")
        .arg("ready-without-stdin")
        .arg("prompt argument")
        .assert()
        .failure()
        .stderr(contains(
            "--invocation-ready-id requires a forced stdin prompt represented by '-'",
        ));

    test.cmd()
        .arg("--json")
        .arg("--invocation-ready-id")
        .arg("ready-without-failover")
        .arg("-")
        .assert()
        .failure()
        .stderr(contains(
            "--invocation-ready-id requires --account-failover pre-semantic",
        ));

    test.cmd()
        .arg("--json")
        .arg("--account-failover=pre-semantic")
        .arg("--invocation-ready-id")
        .arg("ready-fork-rejected")
        .arg("fork")
        .arg("session-1")
        .arg("-")
        .assert()
        .failure()
        .stderr(contains(
            "--invocation-ready-id is supported only for exec and resume invocations",
        ));
}

#[test]
fn exec_without_prompt_argument_rejects_empty_piped_stdin() {
    let test = test_codex_exec();

    // printf "" | codex exec --skip-git-repo-check -C <cwd>
    test.cmd()
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(test.cwd_path())
        .write_stdin("")
        .assert()
        .code(1)
        .stderr(contains("No prompt provided via stdin."));
}

#[test]
fn exec_dash_prompt_rejects_empty_piped_stdin() {
    let test = test_codex_exec();

    // printf "" | codex exec --skip-git-repo-check -C <cwd> -
    test.cmd()
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(test.cwd_path())
        .arg("-")
        .write_stdin("")
        .assert()
        .code(1)
        .stderr(contains("No prompt provided via stdin."));
}
