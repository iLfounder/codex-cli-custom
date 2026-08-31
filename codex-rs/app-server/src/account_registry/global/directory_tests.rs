use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config");
    std::fs::create_dir(&config).unwrap();
    let c1 = home.path().join("account1");
    let c2 = home.path().join("account2");
    std::fs::create_dir(&c1).unwrap();
    std::fs::create_dir(&c2).unwrap();
    (home, config, c1, c2)
}

#[test]
fn source_ref_matches_token_manager_v1_vector() {
    assert_eq!(
        subscription_source_ref(
            "synthetic-codex-account",
            Path::new("/safe/provider-home/codex/account1"),
        ),
        Some("subscription-source-v1:G-fhAxUyHw3v9ePvf2wsKBzaJdC2qS__7m3wjsHrE-A".to_string())
    );
}

#[cfg(unix)]
#[test]
fn valid_private_two_field_registry_resolves_process_account() {
    let (home, config, c1, c2) = fixture();
    let registry = config.join("codex-accounts.tsv");
    std::fs::write(
        &registry,
        format!("1\t{}\n2\t{}\n", c1.display(), c2.display()),
    )
    .unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o600)).unwrap();

    let directory = GlobalAccountDirectory::load_from(home.path(), &c2);
    assert_eq!(directory.homes.len(), 2);
    assert_eq!(directory.process_account_id, AccountId::parse("C2"));
}

#[cfg(unix)]
#[test]
fn unsafe_registry_mode_is_rejected() {
    let (home, config, c1, _) = fixture();
    let registry = config.join("codex-accounts.tsv");
    std::fs::write(&registry, format!("1\t{}\n", c1.display())).unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        GlobalAccountDirectory::load_from(home.path(), &c1).homes,
        BTreeMap::new()
    );
}

#[cfg(unix)]
#[test]
fn registry_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let (home, config, c1, _) = fixture();
    let target = home.path().join("registry-target");
    std::fs::write(&target, format!("1\t{}\n", c1.display())).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(target, config.join("codex-accounts.tsv")).unwrap();

    assert_eq!(
        GlobalAccountDirectory::load_from(home.path(), &c1).homes,
        BTreeMap::new()
    );
}
