use std::io;
use std::path::Path;

use base64::Engine;
use chrono::Utc;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;

use super::ManagedAuthStaging;
use super::managed_auth_state;
use super::remove_managed_auth;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::FileAuthStorage;
use crate::auth::storage::get_auth_file;

fn write_private_chatgpt_auth(
    codex_home: &Path,
    account_id: &str,
    access_token: &str,
) -> io::Result<()> {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header = encode(&serde_json::to_vec(&Header {
        alg: "none",
        typ: "JWT",
    })?);
    let payload = encode(&serde_json::to_vec(&json!({
        "email": "user@example.com",
        "email_verified": true,
        "https://api.openai.com/auth": {
            "chatgpt_user_id": "user-12345",
            "user_id": "user-12345",
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": account_id,
        },
    }))?);
    let id_token = format!("{header}.{payload}.{}", encode(b"sig"));
    std::fs::write(
        get_auth_file(codex_home),
        serde_json::to_vec_pretty(&json!({
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": "test-refresh-token",
                "account_id": account_id,
            },
            "last_refresh": Utc::now(),
        }))?,
    )?;
    let storage = FileAuthStorage::new(codex_home.to_path_buf());
    let auth = storage
        .load()?
        .ok_or_else(|| io::Error::other("test auth was not written"))?;
    storage.save(&auth)
}

#[test]
fn promotion_preserves_existing_credential_on_identity_mismatch_and_cleans_staging() {
    let target = tempfile::tempdir().expect("target home");
    write_private_chatgpt_auth(target.path(), "workspace-a", "old-token").expect("seed target");
    let original = std::fs::read(get_auth_file(target.path())).expect("read original");
    let expected = managed_auth_state(target.path())
        .expect("read current")
        .expect("current state");
    let staging = ManagedAuthStaging::create(target.path()).expect("create staging");
    let staging_home = staging.home().to_path_buf();
    write_private_chatgpt_auth(staging.home(), "workspace-b", "new-token").expect("seed candidate");

    let error = staging
        .promote(Some(&expected))
        .expect_err("reject mismatch");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read(get_auth_file(target.path())).expect("read preserved"),
        original
    );
    assert!(!staging_home.exists());
}

#[test]
fn promotion_rejects_stale_revision_and_cleans_staging() {
    let target = tempfile::tempdir().expect("target home");
    write_private_chatgpt_auth(target.path(), "workspace-a", "first-token").expect("seed target");
    let expected = managed_auth_state(target.path())
        .expect("read current")
        .expect("current state");
    let staging = ManagedAuthStaging::create(target.path()).expect("create staging");
    let staging_home = staging.home().to_path_buf();
    write_private_chatgpt_auth(staging.home(), "workspace-a", "candidate-token")
        .expect("seed candidate");
    write_private_chatgpt_auth(target.path(), "workspace-a", "racing-token")
        .expect("replace target");
    let raced = std::fs::read(get_auth_file(target.path())).expect("read raced credential");

    let error = staging
        .promote(Some(&expected))
        .expect_err("reject stale revision");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(get_auth_file(target.path())).expect("read preserved"),
        raced
    );
    assert!(!staging_home.exists());
}

#[test]
fn logged_out_registration_accepts_first_identity_and_promotes_atomically() {
    let target = tempfile::tempdir().expect("target home");
    let staging = ManagedAuthStaging::create(target.path()).expect("create staging");
    let staging_home = staging.home().to_path_buf();
    write_private_chatgpt_auth(staging.home(), "workspace-new", "candidate-token")
        .expect("seed candidate");

    let promotion = staging.promote(None).expect("promote first login");
    let promoted = promotion.accept();

    assert_eq!(promoted.account_id(), "workspace-new");
    assert!(get_auth_file(target.path()).is_file());
    assert!(!staging_home.exists());
}

#[test]
fn unaccepted_promotion_restores_previous_credential() {
    let target = tempfile::tempdir().expect("target home");
    write_private_chatgpt_auth(target.path(), "workspace-a", "current-token").expect("seed target");
    let original = std::fs::read(get_auth_file(target.path())).expect("read original");
    let expected = managed_auth_state(target.path())
        .expect("read current")
        .expect("current state");
    let staging = ManagedAuthStaging::create(target.path()).expect("create staging");
    write_private_chatgpt_auth(staging.home(), "workspace-a", "candidate-token")
        .expect("seed candidate");

    drop(staging.promote(Some(&expected)).expect("promote candidate"));

    assert_eq!(
        std::fs::read(get_auth_file(target.path())).expect("read restored"),
        original
    );
}

#[test]
fn logout_rejects_stale_revision_without_deleting_current_credential() {
    let target = tempfile::tempdir().expect("target home");
    write_private_chatgpt_auth(target.path(), "workspace-a", "first-token").expect("seed target");
    let expected = managed_auth_state(target.path())
        .expect("read current")
        .expect("current state");
    write_private_chatgpt_auth(target.path(), "workspace-a", "racing-token")
        .expect("replace target");

    let error = remove_managed_auth(target.path(), &expected).expect_err("reject stale logout");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(get_auth_file(target.path()).is_file());
}

#[test]
fn logout_removes_only_the_exact_captured_credential() {
    let target = tempfile::tempdir().expect("target home");
    write_private_chatgpt_auth(target.path(), "workspace-a", "current-token").expect("seed target");
    let expected = managed_auth_state(target.path())
        .expect("read current")
        .expect("current state");

    remove_managed_auth(target.path(), &expected)
        .expect("logout")
        .accept();
    assert!(!get_auth_file(target.path()).exists());
}

#[test]
fn unaccepted_logout_restores_the_exact_credential() {
    let target = tempfile::tempdir().expect("target home");
    write_private_chatgpt_auth(target.path(), "workspace-a", "current-token").expect("seed target");
    let original = std::fs::read(get_auth_file(target.path())).expect("read original");
    let expected = managed_auth_state(target.path())
        .expect("read current")
        .expect("current state");

    drop(remove_managed_auth(target.path(), &expected).expect("stage logout"));

    assert_eq!(
        std::fs::read(get_auth_file(target.path())).expect("read restored"),
        original
    );
}
