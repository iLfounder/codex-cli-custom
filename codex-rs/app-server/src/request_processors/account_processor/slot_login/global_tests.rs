use std::ffi::OsStr;

use pretty_assertions::assert_eq;

use super::GlobalLoginTerminal;
use super::test_oauth_issuer_override;

#[test]
fn global_pre_commit_cancel_prevents_commit_claim() {
    let terminal = GlobalLoginTerminal::default();

    assert!(terminal.request_cancel());
    assert!(!terminal.request_cancel());
    assert!(!terminal.try_begin_commit());
}

#[test]
fn global_commit_claim_is_irreversible_for_late_cancel() {
    let terminal = GlobalLoginTerminal::default();

    assert!(terminal.try_begin_commit());
    assert!(!terminal.request_cancel());
    assert!(!terminal.try_begin_commit());
}

#[test]
fn test_oauth_issuer_accepts_only_root_loopback_http() {
    assert_eq!(
        test_oauth_issuer_override(Some(OsStr::new("http://127.0.0.2:43101"))),
        Ok(Some("http://127.0.0.2:43101".to_string()))
    );
    assert_eq!(
        test_oauth_issuer_override(Some(OsStr::new("http://[::1]:43101/"))),
        Ok(Some("http://[::1]:43101".to_string()))
    );
}

#[test]
fn test_oauth_issuer_rejects_non_loopback_or_non_root_urls() {
    for rejected in [
        "http://localhost:43101",
        "http://192.0.2.1:43101",
        "https://127.0.0.1:43101",
        "file:///tmp/oauth",
        "http://user@127.0.0.1:43101",
        "http://127.0.0.1:43101/path",
        "http://127.0.0.1:43101/?query=value",
        "http://127.0.0.1:43101/#fragment",
        "not a URL",
    ] {
        assert_eq!(
            test_oauth_issuer_override(Some(OsStr::new(rejected))),
            Err(())
        );
    }
    assert_eq!(test_oauth_issuer_override(None), Ok(None));
}

#[cfg(unix)]
#[test]
fn test_oauth_issuer_rejects_non_unicode() {
    use std::os::unix::ffi::OsStrExt;

    assert_eq!(
        test_oauth_issuer_override(Some(OsStr::from_bytes(&[0xff]))),
        Err(())
    );
}
