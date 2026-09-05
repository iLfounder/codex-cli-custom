use super::CatalogError;
use super::MAX_STRING_BYTES;
#[cfg(unix)]
use super::TokenManagerControl;
use super::TokenManagerEvent;
use super::decode_sse_frame;
use crate::account_registry::global::AccountId;
#[cfg(unix)]
use pretty_assertions::assert_eq;

#[test]
fn decodes_frames_and_identifies_only_canonical_snapshot_notification_targets() {
    let initial = br#"event: initial
data: [{"label":"C2","type":"codex-chatgpt","sourceRef":"opaque","fetchedAt":100,"ok":true,"rateLimit":{"status":"allowed","meters":[{"id":"weekly-7d","label":"7D","utilization":0.25,"resetAt":200,"observedAt":100,"utilizationObservedAt":100,"state":"normal"}]}},{"label":"A1","type":"claude-oauth","fetchedAt":100,"ok":true}]

"#;
    let event = decode_sse_frame(initial).expect("decode initial");
    assert_eq!(
        event
            .as_ref()
            .and_then(TokenManagerEvent::snapshot_account_id),
        None
    );
    assert!(matches!(event, Some(TokenManagerEvent::Initial(accounts)) if accounts.len() == 2));

    let snapshot = br#"event: snapshot
data: {"label":"C2","type":"codex-chatgpt","sourceRef":"opaque","fetchedAt":101,"ok":true}

"#;
    let event = decode_sse_frame(snapshot).expect("decode snapshot");
    assert_eq!(
        event
            .as_ref()
            .and_then(TokenManagerEvent::snapshot_account_id),
        AccountId::parse("C2")
    );
    assert!(matches!(event, Some(TokenManagerEvent::Snapshot(account)) if account.label == "C2"));

    let non_codex = br#"event: snapshot
data: {"label":"A1","type":"claude-oauth","fetchedAt":101,"ok":true}

"#;
    let event = decode_sse_frame(non_codex).expect("decode non-Codex snapshot");
    assert_eq!(
        event
            .as_ref()
            .and_then(TokenManagerEvent::snapshot_account_id),
        None
    );
}

#[test]
fn ignores_keepalives_and_unknown_events() {
    assert!(decode_sse_frame(b": ping\n\n").unwrap().is_none());
    assert!(
        decode_sse_frame(b"event: removed\ndata: {}\n\n")
            .unwrap()
            .is_none()
    );
}

#[test]
fn rejects_oversized_strings_before_typed_decode() {
    let oversized = "x".repeat(MAX_STRING_BYTES + 1);
    let frame = format!(
        "event: snapshot\ndata: {{\"label\":\"C1\",\"type\":\"codex-chatgpt\",\"sourceRef\":\"{oversized}\",\"fetchedAt\":100,\"ok\":true}}\n\n"
    );
    assert!(matches!(
        decode_sse_frame(frame.as_bytes()),
        Err(CatalogError::InvalidPayload)
    ));
}

#[cfg(unix)]
fn exercise_control(
    responses: Vec<serde_json::Value>,
) -> (std::io::Result<()>, Vec<serde_json::Value>) {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    let fixture = tempfile::tempdir().expect("control fixture");
    let socket_path = fixture.path().join("tokenmanager.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind control fixture");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept control client");
        let mut connection = BufReader::new(stream);
        let mut requests = Vec::new();
        for response in responses {
            let mut line = String::new();
            connection
                .read_line(&mut line)
                .expect("read control request");
            requests.push(serde_json::from_str(&line).expect("decode control request"));
            serde_json::to_writer(connection.get_mut(), &response)
                .expect("encode control response");
            connection
                .get_mut()
                .write_all(b"\n")
                .expect("terminate control response");
            connection.get_mut().flush().expect("flush response");
        }
        requests
    });

    let result = TokenManagerControl::new(socket_path)
        .force_refresh(AccountId::parse("C4").expect("canonical account"));
    (result, server.join().expect("join control fixture"))
}

#[cfg(unix)]
#[test]
fn force_refresh_uses_one_exact_account_lifecycle_and_commits() {
    let (result, requests) = exercise_control(vec![
        serde_json::json!({"ok": true, "state": "active", "generation": 7}),
        serde_json::json!({"ok": true, "state": "refreshed", "generation": 7}),
        serde_json::json!({"ok": true, "state": "committed"}),
    ]);

    result.expect("force refresh lifecycle");
    assert_eq!(
        requests,
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C4"}),
            serde_json::json!({"method": "lifecycle/forceRefresh", "accountId": "C4"}),
            serde_json::json!({"method": "lifecycle/commit", "accountId": "C4"}),
        ]
    );
}

#[cfg(unix)]
#[test]
fn force_refresh_aborts_the_lifecycle_after_authority_rejection() {
    let (result, requests) = exercise_control(vec![
        serde_json::json!({"ok": true, "state": "active", "generation": 9}),
        serde_json::json!({"ok": false, "code": "refresh_failed"}),
        serde_json::json!({"ok": true, "state": "aborted"}),
    ]);

    assert!(result.is_err());
    assert_eq!(
        requests,
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C4"}),
            serde_json::json!({"method": "lifecycle/forceRefresh", "accountId": "C4"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C4"}),
        ]
    );
}

#[cfg(unix)]
#[test]
fn lifecycle_authority_rejects_same_account_while_other_accounts_remain_independent() {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    let fixture = tempfile::tempdir().expect("control fixture");
    let socket_path = fixture.path().join("tokenmanager.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind control fixture");
    let server = std::thread::spawn(move || {
        let read_request = |stream| {
            let mut connection = BufReader::new(stream);
            let mut line = String::new();
            connection.read_line(&mut line).expect("read request");
            let request: serde_json::Value = serde_json::from_str(&line).expect("decode request");
            (connection, request)
        };
        let respond = |connection: &mut BufReader<std::os::unix::net::UnixStream>, response| {
            serde_json::to_writer(connection.get_mut(), &response).expect("encode response");
            connection
                .get_mut()
                .write_all(b"\n")
                .expect("terminate response");
            connection.get_mut().flush().expect("flush response");
        };

        let (first, _) = listener.accept().expect("accept first lifecycle");
        let (mut first, first_begin) = read_request(first);
        respond(
            &mut first,
            serde_json::json!({"ok": true, "state": "active", "generation": 8}),
        );
        let (same, _) = listener.accept().expect("accept same-account lifecycle");
        let (mut same, same_begin) = read_request(same);
        respond(
            &mut same,
            serde_json::json!({"ok": false, "code": "lifecycle_unavailable"}),
        );
        let (other, _) = listener.accept().expect("accept other lifecycle");
        let (mut other, other_begin) = read_request(other);
        respond(
            &mut other,
            serde_json::json!({"ok": true, "state": "absent", "generation": 0}),
        );

        let mut line = String::new();
        first.read_line(&mut line).expect("read first abort");
        let first_abort = serde_json::from_str(&line).expect("decode first abort");
        respond(
            &mut first,
            serde_json::json!({"ok": true, "state": "aborted"}),
        );
        line.clear();
        other.read_line(&mut line).expect("read other abort");
        let other_abort = serde_json::from_str(&line).expect("decode other abort");
        respond(
            &mut other,
            serde_json::json!({"ok": true, "state": "aborted"}),
        );
        vec![
            first_begin,
            same_begin,
            other_begin,
            first_abort,
            other_abort,
        ]
    });

    let control = TokenManagerControl::new(socket_path);
    let mut first = control
        .begin(AccountId::parse("C1").expect("C1"))
        .expect("begin C1");
    assert!(control.begin(AccountId::parse("C1").expect("C1")).is_err());
    let mut other = control
        .begin(AccountId::parse("C2").expect("C2"))
        .expect("begin C2");
    first.abort_sync();
    other.abort_sync();

    assert_eq!(
        server.join().expect("join control fixture"),
        vec![
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/begin", "accountId": "C2"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C1"}),
            serde_json::json!({"method": "lifecycle/abort", "accountId": "C2"}),
        ]
    );
}
