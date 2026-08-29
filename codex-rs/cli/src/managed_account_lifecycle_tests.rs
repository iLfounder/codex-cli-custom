use std::net::SocketAddr;

use clap::Parser;
use pretty_assertions::assert_eq;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::*;
use crate::MultitoolCli;

fn parsed(args: &[&str]) -> MultitoolCli {
    MultitoolCli::try_parse_from(args).expect("arguments should parse")
}

fn validate(args: &[&str]) -> anyhow::Result<()> {
    let cli = parsed(args);
    let action = lifecycle_action(&cli.subcommand).expect("lifecycle action");
    validate_invocation(
        &cli.config_overrides,
        &cli.feature_toggles,
        &cli.remote,
        &cli.interactive,
        action,
    )
}

#[test]
fn managed_lifecycle_rejects_auth_and_config_overrides() {
    for args in [
        ["codex", "-c", "model=o3", "login"].as_slice(),
        ["codex", "--enable", "unified_exec", "login"].as_slice(),
        ["codex", "--strict-config", "login"].as_slice(),
        ["codex", "--profile", "work", "login"].as_slice(),
        ["codex", "login", "--with-api-key"].as_slice(),
        ["codex", "login", "--with-access-token"].as_slice(),
        ["codex", "login", "--device-auth"].as_slice(),
        [
            "codex",
            "login",
            "--experimental_issuer",
            "https://issuer.invalid",
        ]
        .as_slice(),
        ["codex", "login", "--experimental_client-id", "client"].as_slice(),
    ] {
        assert!(validate(args).is_err(), "accepted {args:?}");
    }
    assert!(validate(&["codex", "login"]).is_ok());
    assert!(validate(&["codex", "login", "status"]).is_ok());
    assert!(validate(&["codex", "logout"]).is_ok());
}

#[test]
fn auth_url_requires_one_exact_loopback_identity() {
    let auth = auth_url("http://127.0.0.1:1455/auth/callback", "state-1");
    let (redirect, state) = validate_auth_url(&auth).expect("valid auth URL");
    assert_eq!(redirect.as_str(), "http://127.0.0.1:1455/auth/callback");
    assert_eq!(state, "state-1");

    for invalid in [
        auth_url("http://example.com:1455/auth/callback", "state-1"),
        auth_url("http://127.0.0.1/auth/callback", "state-1"),
        auth_url("http://127.0.0.1:1455/auth/callback?extra=1", "state-1"),
        auth_url("http://user@127.0.0.1:1455/auth/callback", "state-1"),
    ] {
        assert!(validate_auth_url(&invalid).is_err(), "accepted {invalid}");
    }
}

#[tokio::test]
async fn callback_delivery_accepts_exact_terminal_response() {
    let (address, server) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
    let redirect = format!("http://{address}/auth/callback");
    let auth = auth_url(&redirect, "state-2");
    deliver_callback(&auth, &format!("{redirect}?code=short-lived&state=state-2"))
        .await
        .expect("callback should be delivered");
    server.await.expect("server task");
}

#[tokio::test]
async fn callback_delivery_rejects_mismatch_and_cross_origin_success() {
    let (address, server) = serve_once(
        "HTTP/1.1 302 Found\r\nLocation: http://example.com/success?secret=hidden\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    let redirect = format!("http://{address}/auth/callback");
    let auth = auth_url(&redirect, "state-3");
    let callback = format!("{redirect}?code=short-lived&state=state-3");
    let error = deliver_callback(&auth, &callback)
        .await
        .expect_err("cross-origin success must fail");
    assert!(!error.to_string().contains("secret=hidden"));
    server.await.expect("server task");

    for invalid in [
        format!("{redirect}?code=x&state=wrong"),
        format!("{redirect}?code=x&error=denied&state=state-3"),
        "http://127.0.0.1:1/auth/callback?code=x&state=state-3".to_string(),
    ] {
        assert!(deliver_callback(&auth, &invalid).await.is_err());
    }
}

fn auth_url(redirect: &str, state: &str) -> String {
    let mut auth = Url::parse("https://auth.openai.example/authorize").expect("auth base");
    auth.query_pairs_mut()
        .append_pair("redirect_uri", redirect)
        .append_pair("state", state);
    auth.into()
}

async fn serve_once(response: &'static str) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await.expect("read request");
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    (address, task)
}
