use std::fs;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::ManagedAccountCatalog;
use super::ManagedAccountCatalogError;
use super::ManagedAccountHintError;
use super::ManagedAccountId;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let owner_home = tempfile::tempdir().expect("owner home");
    let config = owner_home.path().join(".config");
    fs::create_dir(&config).expect("config dir");
    let c1 = owner_home.path().join("account1");
    let c2 = owner_home.path().join("account2");
    fs::create_dir(&c1).expect("C1 home");
    fs::create_dir(&c2).expect("C2 home");
    (owner_home, config, c1, c2)
}

#[cfg(unix)]
fn write_registry(config: &std::path::Path, contents: &str) {
    let registry = config.join("codex-accounts.tsv");
    fs::write(&registry, contents).expect("registry");
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).expect("private registry");
}

#[cfg(unix)]
#[test]
fn private_catalog_canonicalizes_entries_and_matches_exact_hint_home() {
    let (owner_home, config, c1, c2) = fixture();
    write_registry(
        &config,
        &format!("1\t{}\n2\t{}\n", c1.display(), c2.display()),
    );

    let catalog = ManagedAccountCatalog::load_from_owner_home(owner_home.path())
        .expect("valid private catalog");

    assert_eq!(
        catalog
            .entries()
            .map(|(account_id, home)| (account_id, home.to_path_buf()))
            .collect::<Vec<_>>(),
        vec![
            (
                ManagedAccountId::parse("C1").expect("C1"),
                fs::canonicalize(&c1).expect("canonical C1"),
            ),
            (
                ManagedAccountId::parse("C2").expect("C2"),
                fs::canonicalize(&c2).expect("canonical C2"),
            ),
        ]
    );
    assert_eq!(
        catalog.match_hint("C2", &c2),
        Ok(ManagedAccountId::parse("C2").expect("C2"))
    );
    assert_eq!(
        catalog.match_hint("C2", &c1),
        Err(ManagedAccountHintError::CodexHomeMismatch)
    );
    assert_eq!(
        catalog.match_hint("C3", &c1),
        Err(ManagedAccountHintError::UnknownAccount)
    );
    assert_eq!(
        catalog.match_hint("03", &c1),
        Err(ManagedAccountHintError::MalformedHint)
    );
}

#[cfg(unix)]
#[test]
fn unsafe_or_duplicate_catalog_is_rejected_as_a_whole() {
    let (owner_home, config, c1, _) = fixture();
    let registry = config.join("codex-accounts.tsv");
    fs::write(&registry, format!("1\t{}\n", c1.display())).expect("registry");
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o644)).expect("unsafe mode");
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::UnsafeFile)
    );

    write_registry(
        &config,
        &format!("1\t{}\n2\t{}\n", c1.display(), c1.display()),
    );
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::DuplicateEntry)
    );
}

#[cfg(unix)]
#[test]
fn catalog_requires_exact_mode_c1_and_existing_homes() {
    let (owner_home, config, c1, c2) = fixture();
    let registry = config.join("codex-accounts.tsv");
    fs::write(&registry, format!("1\t{}\n", c1.display())).expect("registry");
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o400)).expect("read-only mode");
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::UnsafeFile)
    );
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).expect("writable mode");

    write_registry(&config, &format!("2\t{}\n", c2.display()));
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::InvalidEntry)
    );

    write_registry(
        &config,
        &format!(
            "1\t{}\n2\t{}\n",
            c1.display(),
            owner_home.path().join("missing").display()
        ),
    );
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::InvalidEntry)
    );
}

#[cfg(unix)]
#[test]
fn catalog_rejects_leading_zero_and_final_home_symlink() {
    use std::os::unix::fs::symlink;

    let (owner_home, config, c1, c2) = fixture();
    write_registry(
        &config,
        &format!("1\t{}\n02\t{}\n", c1.display(), c2.display()),
    );
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::InvalidEntry)
    );

    write_registry(&config, "1\taccount1\n");
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::InvalidEntry)
    );

    let linked_home = owner_home.path().join("linked-account2");
    symlink(&c2, &linked_home).expect("home symlink");
    write_registry(
        &config,
        &format!("1\t{}\n2\t{}\n", c1.display(), linked_home.display()),
    );
    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::InvalidEntry)
    );
}

#[cfg(unix)]
#[test]
fn catalog_allows_symlinked_ancestor_and_stores_physical_home() {
    use std::os::unix::fs::symlink;

    let physical_owner = tempfile::tempdir().expect("physical owner");
    let physical_config = physical_owner.path().join(".config");
    fs::create_dir(&physical_config).expect("physical config");
    let physical_c1 = physical_owner.path().join("account1");
    fs::create_dir(&physical_c1).expect("physical C1");
    let link_root = tempfile::tempdir().expect("link root");
    let linked_owner = link_root.path().join("owner");
    symlink(physical_owner.path(), &linked_owner).expect("owner symlink");
    write_registry(
        &physical_config,
        &format!("1\t{}\n", linked_owner.join("account1").display()),
    );

    let catalog = ManagedAccountCatalog::load_from_owner_home(&linked_owner)
        .expect("symlinked ancestor is canonicalized");
    assert_eq!(
        catalog.home(ManagedAccountId::from_number(1).expect("C1")),
        Some(
            fs::canonicalize(physical_c1)
                .expect("physical C1")
                .as_path()
        )
    );
}

#[cfg(unix)]
#[test]
fn catalog_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let (owner_home, config, c1, _) = fixture();
    let target = owner_home.path().join("registry-target");
    fs::write(&target, format!("1\t{}\n", c1.display())).expect("target registry");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("private target");
    symlink(target, config.join("codex-accounts.tsv")).expect("registry symlink");

    assert_eq!(
        ManagedAccountCatalog::load_from_owner_home(owner_home.path()),
        Err(ManagedAccountCatalogError::Unavailable)
    );
}
