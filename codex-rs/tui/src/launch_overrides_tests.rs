use std::fs;

use clap::Parser;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

struct Fixture {
    _owner_home: TempDir,
    codex_home: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let owner_home = tempfile::tempdir().expect("owner home");
        let codex_home = owner_home.path().join(".codex-account1");
        fs::create_dir_all(&codex_home).expect("account home");
        fs::create_dir_all(owner_home.path().join(".config")).expect("config directory");
        fs::write(
            owner_home.path().join(".config/codex-accounts.tsv"),
            format!("1\t{}\n", codex_home.display()),
        )
        .expect("account catalog");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                owner_home.path().join(".config/codex-accounts.tsv"),
                fs::Permissions::from_mode(0o600),
            )
            .expect("catalog permissions");
        }
        Self {
            _owner_home: owner_home,
            codex_home,
        }
    }
}

#[test]
fn absent_hint_preserves_unmanaged_upstream() {
    let fixture = Fixture::new();
    assert_eq!(
        classify_launch(
            LaunchClassification {
                managed_account_hint: None,
                codex_home: &fixture.codex_home,
                operation: LaunchOperation::NewThread,
                requested_mode: RequestedLaunchMode::StandardLocal,
                overrides: InvocationOverrides::EmbeddedOnly,
            },
            &ManagedAccountCatalog::default()
        )
        .expect("unmanaged launch"),
        LaunchDisposition::UnmanagedUpstream
    );
}

#[test]
fn matched_managed_launch_is_canonical() {
    let fixture = Fixture::new();
    let catalog = ManagedAccountCatalog::load_from_owner_home(fixture._owner_home.path())
        .expect("owner catalog");
    let input = LaunchClassification {
        managed_account_hint: Some("C1"),
        codex_home: &fixture.codex_home,
        operation: LaunchOperation::NewThread,
        requested_mode: RequestedLaunchMode::StandardLocal,
        overrides: InvocationOverrides::None,
    };
    assert_eq!(
        classify_launch(input, &catalog).expect("managed launch"),
        LaunchDisposition::CanonicalLocal
    );
}

#[test]
fn existing_thread_never_accepts_explicit_embedded() {
    let fixture = Fixture::new();
    let catalog = ManagedAccountCatalog::load_from_owner_home(fixture._owner_home.path())
        .expect("owner catalog");
    let err = classify_launch(
        LaunchClassification {
            managed_account_hint: Some("C1"),
            codex_home: &fixture.codex_home,
            operation: LaunchOperation::ExistingThread,
            requested_mode: RequestedLaunchMode::ExplicitEmbedded,
            overrides: InvocationOverrides::None,
        },
        &catalog,
    )
    .expect_err("embedded existing thread");
    assert!(err.to_string().contains("existing managed threads"));
}

#[test]
fn managed_safe_override_uses_canonical_projection() {
    let fixture = Fixture::new();
    let catalog = ManagedAccountCatalog::load_from_owner_home(fixture._owner_home.path())
        .expect("owner catalog");
    assert_eq!(
        classify_launch(
            LaunchClassification {
                managed_account_hint: Some("C1"),
                codex_home: &fixture.codex_home,
                operation: LaunchOperation::NewThread,
                requested_mode: RequestedLaunchMode::StandardLocal,
                overrides: InvocationOverrides::CanonicalSafe,
            },
            &catalog,
        )
        .expect("safe canonical override"),
        LaunchDisposition::CanonicalLocal
    );
}

#[test]
fn managed_remote_and_workload_modes_keep_their_product_boundary() {
    let fixture = Fixture::new();
    let catalog = ManagedAccountCatalog::load_from_owner_home(fixture._owner_home.path())
        .expect("owner catalog");
    for (requested_mode, expected) in [
        (
            RequestedLaunchMode::ExplicitRemote,
            LaunchDisposition::ExplicitRemote,
        ),
        (
            RequestedLaunchMode::WorkloadIdentity,
            LaunchDisposition::WorkloadIdentity,
        ),
    ] {
        assert_eq!(
            classify_launch(
                LaunchClassification {
                    managed_account_hint: Some("C1"),
                    codex_home: &fixture.codex_home,
                    operation: LaunchOperation::ExistingThread,
                    requested_mode,
                    overrides: InvocationOverrides::EmbeddedOnly,
                },
                &catalog,
            )
            .expect("special product mode"),
            expected
        );
    }
}

#[test]
fn malformed_unknown_and_home_mismatch_fail_closed() {
    let fixture = Fixture::new();
    let other_home = fixture._owner_home.path().join(".codex-account2");
    fs::create_dir(&other_home).expect("other home");
    let catalog = ManagedAccountCatalog::load_from_owner_home(fixture._owner_home.path())
        .expect("owner catalog");

    for (hint, home) in [
        ("1", fixture.codex_home.as_path()),
        ("C2", fixture.codex_home.as_path()),
        ("C1", other_home.as_path()),
    ] {
        classify_launch(
            LaunchClassification {
                managed_account_hint: Some(hint),
                codex_home: home,
                operation: LaunchOperation::NewThread,
                requested_mode: RequestedLaunchMode::StandardLocal,
                overrides: InvocationOverrides::None,
            },
            &catalog,
        )
        .expect_err("invalid managed launch");
    }
}

#[test]
fn safe_and_unsafe_cli_inputs_are_distinguished() {
    let safe = Cli::parse_from(["codex", "--model", "gpt-test"]);
    assert_eq!(
        invocation_overrides(&safe, &[], &LoaderOverrides::default()),
        InvocationOverrides::CanonicalSafe
    );

    let unsafe_profile = Cli::parse_from(["codex", "--profile", "work"]);
    assert_eq!(
        invocation_overrides(&unsafe_profile, &[], &LoaderOverrides::default()),
        InvocationOverrides::EmbeddedOnly
    );
    let unsafe_config = vec![(
        "mcp_servers.docs.command".to_string(),
        toml::Value::String("custom".to_string()),
    )];
    assert_eq!(
        invocation_overrides(
            &Cli::parse_from(["codex"]),
            &unsafe_config,
            &LoaderOverrides::default()
        ),
        InvocationOverrides::EmbeddedOnly
    );
}
