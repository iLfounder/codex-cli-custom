//! Small Windows security-descriptor helpers for owner-local Codex state.
//!
//! Files used for credentials, account selection, and local IPC must not rely
//! on the ACL inherited from an arbitrary parent directory.  The Unix callers
//! enforce the equivalent mode/uid policy themselves; this module supplies the
//! Windows owner/DACL check and repair used by those callers.

use std::io;
use std::path::Path;

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Foundation::PSID;
    use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
    use windows_sys::Win32::Security::ACE_HEADER;
    use windows_sys::Win32::Security::ACL;
    use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
    use windows_sys::Win32::Security::AclSizeInformation;
    use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
    use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Security::Authorization::SET_ACCESS;
    use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
    use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
    use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
    use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
    use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::Security::GetAce;
    use windows_sys::Win32::Security::GetAclInformation;
    use windows_sys::Win32::Security::GetLengthSid;
    use windows_sys::Win32::Security::GetTokenInformation;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::Security::TOKEN_USER;
    use windows_sys::Win32::Security::TokenUser;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
    use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const INHERIT_ONLY_ACE: u8 = 0x08;
    const CONTAINER_INHERIT_ACE: u32 = 0x02;
    const OBJECT_INHERIT_ACE: u32 = 0x01;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct FileIdentity {
        volume_serial: u32,
        file_index: u64,
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    pub(super) fn file_identity(path: &Path) -> io::Result<FileIdentity> {
        let name = wide_path(path);
        // SAFETY: `name` is NUL terminated; the returned handle is closed
        // before this function returns.
        unsafe {
            let handle = windows_sys::Win32::Storage::FileSystem::CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                0,
            );
            if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            let result = if GetFileInformationByHandle(handle, &mut info) == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(FileIdentity {
                    volume_serial: info.dwVolumeSerialNumber,
                    file_index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
                })
            };
            CloseHandle(handle);
            result
        }
    }

    pub(super) fn file_identity_from_file(file: &std::fs::File) -> io::Result<FileIdentity> {
        // SAFETY: `file` owns a valid handle for the duration of this call.
        unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            if GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(FileIdentity {
                volume_serial: info.dwVolumeSerialNumber,
                file_index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            })
        }
    }

    fn win32_error(code: u32, operation: &str, path: &Path) -> io::Error {
        let code = if code == 0 {
            unsafe { GetLastError() }
        } else {
            code
        };
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{operation} failed for {} (Windows error {code})",
                path.display()
            ),
        )
    }

    /// Return a stable copy of the current process' user SID.
    fn current_user_sid() -> io::Result<Vec<u8>> {
        // SAFETY: all pointers are either null for the documented size query
        // or point at buffers sized from the preceding query.
        unsafe {
            let mut token = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let result = (|| {
                let mut required = 0u32;
                GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
                if required == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut buffer = vec![0u8; required as usize];
                if GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr() as *mut c_void,
                    required,
                    &mut required,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if buffer.len() < size_of::<TOKEN_USER>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Windows token user information is truncated",
                    ));
                }
                let token_user = ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER);
                let sid = token_user.User.Sid;
                if sid.is_null() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Windows token did not contain a valid user SID",
                    ));
                }
                let sid_len = GetLengthSid(sid);
                if sid_len == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Windows token did not contain a valid user SID",
                    ));
                }
                let mut sid_bytes = vec![0u8; sid_len as usize];
                if windows_sys::Win32::Security::CopySid(
                    sid_len,
                    sid_bytes.as_mut_ptr() as PSID,
                    sid,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(sid_bytes)
            })();
            CloseHandle(token);
            result
        }
    }

    /// Invoke `f` while the security descriptor returned by Windows is alive.
    fn with_security<T>(path: &Path, f: impl FnOnce(PSID, *mut ACL) -> T) -> io::Result<T> {
        let name = wide_path(path);
        // SAFETY: `name` is NUL terminated and all output pointers are valid.
        unsafe {
            let mut owner: PSID = ptr::null_mut();
            let mut dacl: *mut ACL = ptr::null_mut();
            let mut descriptor: *mut c_void = ptr::null_mut();
            let code = GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            );
            if code != ERROR_SUCCESS {
                if !descriptor.is_null() {
                    LocalFree(descriptor as HLOCAL);
                }
                return Err(win32_error(code, "GetNamedSecurityInfoW", path));
            }
            let result = f(owner, dacl);
            if !descriptor.is_null() {
                LocalFree(descriptor as HLOCAL);
            }
            Ok(result)
        }
    }

    fn dacl_is_owner_only(dacl: *mut ACL, owner: PSID) -> bool {
        if dacl.is_null() || owner.is_null() {
            return false;
        }
        // SAFETY: `dacl` comes from GetNamedSecurityInfoW and remains valid for
        // this call.  GetAce/GetAclInformation validate the ACL layout.
        unsafe {
            let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
            if GetAclInformation(
                dacl as *const ACL,
                &mut info as *mut _ as *mut c_void,
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ) == 0
            {
                return false;
            }
            let mut owner_allow = false;
            for index in 0..info.AceCount {
                let mut ace_ptr: *mut c_void = ptr::null_mut();
                if GetAce(dacl as *const ACL, index, &mut ace_ptr) == 0 || ace_ptr.is_null() {
                    return false;
                }
                let header = &*(ace_ptr as *const ACE_HEADER);
                if header.AceType != ACCESS_ALLOWED_ACE_TYPE
                    || (header.AceFlags & INHERIT_ONLY_ACE) != 0
                {
                    // Explicit deny ACEs do not broaden access.  Unknown ACE
                    // types are rejected because they may grant another SID.
                    if header.AceType != 1 {
                        return false;
                    }
                    continue;
                }
                let allowed = &*(ace_ptr as *const ACCESS_ALLOWED_ACE);
                let sid_ptr =
                    (ace_ptr as *const u8).add(size_of::<ACE_HEADER>() + size_of::<u32>()) as PSID;
                if EqualSid(sid_ptr, owner) == 0 {
                    return false;
                }
                owner_allow = true;
                // A reduced owner ACE is still private, but callers may fail
                // naturally if they lack the operation-specific right.
                let _ = allowed.Mask;
            }
            owner_allow
        }
    }

    pub(super) fn is_owner_private(path: &Path) -> io::Result<bool> {
        let user_sid = current_user_sid()?;
        with_security(path, |owner, dacl| unsafe {
            !owner.is_null()
                && windows_sys::Win32::Security::EqualSid(owner, user_sid.as_ptr() as PSID) != 0
                && dacl_is_owner_only(dacl, owner)
        })
    }

    pub(super) fn ensure_owner_private(path: &Path) -> io::Result<()> {
        let user_sid = current_user_sid()?;
        let (owner_matches, already_private) = with_security(path, |owner, dacl| unsafe {
            (
                !owner.is_null()
                    && windows_sys::Win32::Security::EqualSid(owner, user_sid.as_ptr() as PSID)
                        != 0,
                !owner.is_null() && dacl_is_owner_only(dacl, owner),
            )
        })?;
        if !owner_matches {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not owned by the current Windows user",
                    path.display()
                ),
            ));
        }
        if already_private {
            return Ok(());
        }

        let name = wide_path(path);
        let trustee = TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: user_sid.as_ptr() as *mut u16,
        };
        let explicit = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
            Trustee: trustee,
        };
        // SAFETY: `explicit` references the stable SID buffer for the duration
        // of the call; Windows allocates the returned ACL for LocalFree.
        unsafe {
            let mut new_dacl: *mut ACL = ptr::null_mut();
            let code = SetEntriesInAclW(1, &explicit, ptr::null(), &mut new_dacl);
            if code != ERROR_SUCCESS {
                return Err(win32_error(code, "SetEntriesInAclW", path));
            }
            let code = SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                new_dacl,
                ptr::null_mut(),
            );
            if !new_dacl.is_null() {
                LocalFree(new_dacl as HLOCAL);
            }
            if code != ERROR_SUCCESS {
                return Err(win32_error(code, "SetNamedSecurityInfoW", path));
            }
        }
        if !is_owner_private(path)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} did not receive an owner-only Windows ACL",
                    path.display()
                ),
            ));
        }
        Ok(())
    }
}

/// Returns whether `path` is owned by the current Windows user and has no
/// non-owner allow ACEs.  Non-Windows callers retain their existing policy and
/// receive `true` here; Unix mode/uid checks remain at their call sites.
pub fn is_owner_private(path: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    {
        windows::is_owner_private(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(true)
    }
}

/// Protect an existing file or directory with an owner-only Windows DACL.
/// The operation is intentionally fail-closed when the current user is not
/// already the object owner.
pub fn ensure_owner_private(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::ensure_owner_private(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(())
    }
}

/// Stable Windows file identity helpers used to detect path replacement races.
#[cfg(windows)]
pub use windows::FileIdentity;

#[cfg(windows)]
pub fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    windows::file_identity(path)
}

#[cfg(windows)]
pub fn file_identity_from_file(file: &std::fs::File) -> io::Result<FileIdentity> {
    windows::file_identity_from_file(file)
}
