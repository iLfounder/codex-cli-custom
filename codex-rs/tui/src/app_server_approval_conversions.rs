//! Narrow conversion helpers for approval-related app-server payloads.
//!
//! The TUI mostly keeps app-server approval types intact. These helpers cover
//! the remaining cases where the UI consumes a private file-change display
//! model or needs to translate a granted permission response for outbound
//! submission.

use crate::diff_model::FileChange;
use codex_app_server_protocol::AdditionalNetworkPermissions;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::GrantedPermissionProfile;
use codex_app_server_protocol::PatchChangeKind;
use codex_protocol::request_permissions::RequestPermissionProfile as CoreRequestPermissionProfile;
use codex_utils_path_uri::LegacyAppPathString;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn granted_permission_profile_from_request(
    value: CoreRequestPermissionProfile,
) -> GrantedPermissionProfile {
    GrantedPermissionProfile {
        network: value.network.map(|network| AdditionalNetworkPermissions {
            enabled: network.enabled,
        }),
        file_system: value.file_system.map(Into::into),
    }
}

pub(crate) fn file_update_changes_to_display(
    changes: Vec<FileUpdateChange>,
) -> HashMap<PathBuf, FileChange> {
    changes
        .into_iter()
        .map(|change| {
            // This model is display-only: preserve relative and foreign-host
            // app-server spellings instead of requiring a host-native path.
            let path = PathBuf::from(change.path.into_string());
            let file_change = match change.kind {
                PatchChangeKind::Add => FileChange::Add {
                    content: change.diff,
                },
                PatchChangeKind::Delete => FileChange::Delete {
                    content: change.diff,
                },
                PatchChangeKind::Update { move_path } => FileChange::Update {
                    unified_diff: change.diff,
                    move_path: move_path.map(|path| PathBuf::from(path.into_string())),
                },
            };
            (path, file_change)
        })
        .collect()
}

/// Renders an app-server file update as a concise, cross-platform path label.
///
/// Relative paths stay relative. Absolute paths inside the execution host's
/// cwd are shortened lexically without interpreting them using the TUI host's
/// native path rules.
pub(crate) fn file_update_path_for_display(cwd: &LegacyAppPathString, path: &Path) -> String {
    let path = LegacyAppPathString::from_path(path);
    let Some(path_uri) = path.to_inferred_path_uri() else {
        return path.render_for_ui();
    };
    cwd.to_inferred_path_uri()
        .and_then(|cwd| path_uri.relative_path_from(&cwd))
        .unwrap_or_else(|| path_uri.inferred_native_path_string())
}

/// Renders an app-server file update as an execution-host destination.
///
/// `PathBuf` is retained by the private diff model for compatibility, but it
/// may contain path syntax from a different OS. Parse and join it through the
/// portable app path types rather than the TUI host's native path rules.
pub(crate) fn file_update_destination_for_display(
    cwd: &LegacyAppPathString,
    path: &Path,
) -> String {
    let path = LegacyAppPathString::from_path(path);
    path.to_inferred_path_uri()
        .or_else(|| {
            cwd.to_inferred_path_uri()
                .and_then(|cwd| cwd.join(path.as_str()).ok())
        })
        .map(|path| path.inferred_native_path_string())
        .unwrap_or_else(|| path.render_for_ui())
}

#[cfg(test)]
mod tests {
    use super::file_update_changes_to_display;
    use super::file_update_destination_for_display;
    use super::file_update_path_for_display;
    use super::granted_permission_profile_from_request;
    use crate::diff_model::FileChange;
    use codex_app_server_protocol::AdditionalFileSystemPermissions;
    use codex_app_server_protocol::AdditionalNetworkPermissions;
    use codex_app_server_protocol::FileSystemAccessMode;
    use codex_app_server_protocol::FileSystemPath;
    use codex_app_server_protocol::FileSystemSandboxEntry;
    use codex_app_server_protocol::FileSystemSpecialPath;
    use codex_app_server_protocol::FileUpdateChange;
    use codex_app_server_protocol::GrantedPermissionProfile;
    use codex_app_server_protocol::PatchChangeKind;
    use codex_app_server_protocol::RequestPermissionProfile;
    use codex_protocol::request_permissions::RequestPermissionProfile as CoreRequestPermissionProfile;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn absolute_path(path: &str) -> AbsolutePathBuf {
        AbsolutePathBuf::try_from(PathBuf::from(path)).expect("path must be absolute")
    }

    #[test]
    fn converts_file_update_changes_to_display() {
        assert_eq!(
            file_update_changes_to_display(vec![FileUpdateChange {
                path: codex_utils_path_uri::LegacyAppPathString::from_string("foo.txt"),
                kind: PatchChangeKind::Add,
                diff: "hello\n".to_string(),
            }]),
            HashMap::from([(
                PathBuf::from("foo.txt"),
                FileChange::Add {
                    content: "hello\n".to_string(),
                },
            )])
        );
    }

    #[test]
    fn preserves_foreign_file_update_paths_for_display() {
        let changes = file_update_changes_to_display(vec![FileUpdateChange {
            path: codex_utils_path_uri::LegacyAppPathString::from_string("/srv/project/foo.txt"),
            kind: PatchChangeKind::Update {
                move_path: Some(codex_utils_path_uri::LegacyAppPathString::from_string(
                    r"C:\workspace\bar.txt",
                )),
            },
            diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
        }]);

        assert_eq!(
            changes,
            HashMap::from([(
                PathBuf::from("/srv/project/foo.txt"),
                FileChange::Update {
                    unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                    move_path: Some(PathBuf::from(r"C:\workspace\bar.txt")),
                },
            )])
        );
    }

    #[test]
    fn renders_relative_file_update_against_foreign_cwd() {
        #[cfg(windows)]
        let (cwd, expected) = ("/Users/daniel/project", "/Users/daniel/project/src/lib.rs");
        #[cfg(not(windows))]
        let (cwd, expected) = (
            r"C:\Users\Daniel\project",
            r"C:\Users\Daniel\project\src\lib.rs",
        );

        assert_eq!(
            file_update_destination_for_display(
                &codex_utils_path_uri::LegacyAppPathString::from_string(cwd),
                std::path::Path::new("src/lib.rs"),
            ),
            expected
        );
    }

    #[test]
    fn keeps_relative_file_update_label_relative() {
        assert_eq!(
            file_update_path_for_display(
                &codex_utils_path_uri::LegacyAppPathString::from_string(r"C:\workspace\project",),
                std::path::Path::new("src/lib.rs"),
            ),
            "src/lib.rs"
        );
    }

    #[test]
    fn shortens_foreign_absolute_file_update_label_against_cwd() {
        #[cfg(windows)]
        let (cwd, path, expected) = (
            "/Users/daniel/project",
            "/Users/daniel/project/src/lib.rs",
            "src/lib.rs",
        );
        #[cfg(not(windows))]
        let (cwd, path, expected) = (
            r"C:\Users\Daniel\project",
            r"C:\Users\Daniel\project\src\lib.rs",
            r"src\lib.rs",
        );

        assert_eq!(
            file_update_path_for_display(
                &codex_utils_path_uri::LegacyAppPathString::from_string(cwd),
                std::path::Path::new(path),
            ),
            expected
        );
    }

    #[test]
    fn converts_request_permissions_into_granted_permissions() {
        let request = RequestPermissionProfile {
            network: Some(AdditionalNetworkPermissions {
                enabled: Some(true),
            }),
            file_system: Some(AdditionalFileSystemPermissions {
                read: Some(vec![absolute_path("/tmp/read-only").into()]),
                write: Some(vec![absolute_path("/tmp/write").into()]),
                glob_scan_max_depth: None,
                entries: None,
            }),
        };
        let request = CoreRequestPermissionProfile::try_from(request)
            .expect("API paths should convert to native paths");

        assert_eq!(
            granted_permission_profile_from_request(request),
            GrantedPermissionProfile {
                network: Some(AdditionalNetworkPermissions {
                    enabled: Some(true),
                }),
                file_system: Some(AdditionalFileSystemPermissions {
                    read: Some(vec![absolute_path("/tmp/read-only").into()]),
                    write: Some(vec![absolute_path("/tmp/write").into()]),
                    glob_scan_max_depth: None,
                    entries: Some(vec![
                        FileSystemSandboxEntry {
                            path: FileSystemPath::Path {
                                path: absolute_path("/tmp/read-only").into(),
                            },
                            access: FileSystemAccessMode::Read,
                        },
                        FileSystemSandboxEntry {
                            path: FileSystemPath::Path {
                                path: absolute_path("/tmp/write").into(),
                            },
                            access: FileSystemAccessMode::Write,
                        },
                    ]),
                }),
            }
        );
    }

    #[test]
    fn converts_request_permissions_into_canonical_granted_permissions() {
        let request = RequestPermissionProfile {
            network: None,
            file_system: Some(AdditionalFileSystemPermissions {
                read: None,
                write: None,
                glob_scan_max_depth: None,
                entries: Some(vec![FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    access: FileSystemAccessMode::Write,
                }]),
            }),
        };
        let request = CoreRequestPermissionProfile::try_from(request)
            .expect("API paths should convert to native paths");

        assert_eq!(
            granted_permission_profile_from_request(request),
            GrantedPermissionProfile {
                network: None,
                file_system: Some(AdditionalFileSystemPermissions {
                    read: None,
                    write: None,
                    glob_scan_max_depth: None,
                    entries: Some(vec![FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::Root,
                        },
                        access: FileSystemAccessMode::Write,
                    }]),
                }),
            }
        );
    }
}
