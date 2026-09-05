use super::find_owner_home_from_env;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::io::ErrorKind;
use tempfile::TempDir;

#[test]
fn explicit_owner_is_canonicalized_without_consulting_os_owner() {
    let owner = TempDir::new().expect("temporary owner");
    let nested = owner.path().join("nested");
    std::fs::create_dir(&nested).expect("create nested directory");
    let input = nested.join("..");

    let resolved = find_owner_home_from_env(Some(input.as_os_str()), || {
        panic!("an explicit owner must not consult OS discovery")
    })
    .expect("resolve isolated owner");

    let expected =
        AbsolutePathBuf::from_absolute_path(owner.path().canonicalize().expect("canonical owner"))
            .expect("absolute owner");
    assert_eq!(resolved, expected);
}

#[test]
fn invalid_explicit_owner_never_falls_back_to_os_owner() {
    let owner = TempDir::new().expect("temporary owner");
    let missing = owner.path().join("missing");
    let file = owner.path().join("file");
    std::fs::write(&file, "not a directory").expect("create file");

    for input in [
        OsStr::new(""),
        OsStr::new("relative-owner"),
        missing.as_os_str(),
        file.as_os_str(),
    ] {
        let error = find_owner_home_from_env(Some(input), || {
            panic!("an invalid explicit owner must not consult OS discovery")
        })
        .expect_err("reject invalid owner");
        // Account startup treats only absent OS discovery (NotFound) as
        // optional; every explicit override failure must remain fatal.
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
}

#[test]
fn absent_override_preserves_os_owner_discovery_and_its_absence() {
    let owner = TempDir::new().expect("temporary OS owner");
    let expected = AbsolutePathBuf::from_absolute_path(owner.path()).expect("absolute owner");
    let resolved = find_owner_home_from_env(
        /*owner_home_env*/ None,
        || Some(owner.path().to_path_buf()),
    )
    .expect("resolve OS owner");
    assert_eq!(resolved, expected);

    let error = find_owner_home_from_env(/*owner_home_env*/ None, || None)
        .expect_err("OS owner unavailable");
    assert_eq!(error.kind(), ErrorKind::NotFound);
}
