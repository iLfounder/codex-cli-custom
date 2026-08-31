use super::CatalogError;
use super::MAX_STRING_BYTES;
use super::TokenManagerEvent;
use super::decode_sse_frame;
use crate::account_registry::global::AccountId;

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
