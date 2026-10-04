//! Windows access control for xssh's private files and the daemon pipe: owner-only DACLs
//! (the current user, SYSTEM and Administrators), and checking who owns a pipe server.

use std::io;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};

const SDDL_REVISION_1: u32 = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// String SID (`S-1-5-21-...`) of the user owning a process token.
fn token_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let mut len = 0u32;
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut len) };
    let mut buf = vec![0u8; len.max(64) as usize];
    if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), buf.len() as u32, &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation(TokenUser) filled the buffer with a TOKEN_USER.
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut s: windows_sys::core::PWSTR = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut n = 0;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    unsafe { LocalFree(s.cast()) };
    Ok(out)
}

pub fn current_user_sid() -> io::Result<String> {
    token_user_sid(unsafe { GetCurrentProcess() })
}

/// SID of the user running process `pid`.
pub fn process_user_sid(pid: u32) -> io::Result<String> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    let h = Handle(h);
    token_user_sid(h.0)
}

fn sid_string(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
    let mut s: windows_sys::core::PWSTR = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut n = 0;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    unsafe { LocalFree(s.cast()) };
    Ok(out)
}

/// Owner SID of a kernel object (e.g. the pipe instance a client connected to).
/// # Safety
/// `h` must be a valid, open handle for the duration of the call.
pub unsafe fn handle_owner_sid(h: HANDLE) -> io::Result<String> {
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    let mut owner: windows_sys::Win32::Security::PSID = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    let rc = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let r = sid_string(owner);
    unsafe { LocalFree(sd.cast()) };
    r
}

/// Is a pipe created by this user? Objects created from an elevated token are owned by
/// Administrators instead of the user; both count as ours.
/// # Safety
/// `h` must be a valid, open handle for the duration of the call.
pub unsafe fn owned_by_us(h: HANDLE) -> io::Result<(bool, String)> {
    let owner = unsafe { handle_owner_sid(h)? };
    let ok = owner == current_user_sid()? || owner == "S-1-5-32-544" || owner == "S-1-5-18";
    Ok((ok, owner))
}

/// Owner-only access: full control for the current user, SYSTEM and Administrators; not
/// inherited from the parent. Directories pass it on to what is created inside them.
pub fn owner_only_sddl(dir: bool) -> io::Result<String> {
    let sid = current_user_sid()?;
    let inh = if dir { "OICI" } else { "" };
    Ok(format!("D:P(A;{inh};FA;;;{sid})(A;{inh};FA;;;SY)(A;{inh};FA;;;BA)"))
}

/// Security descriptor for the daemon pipe: only the current user (and SYSTEM) may connect.
pub fn pipe_sddl() -> io::Result<String> {
    Ok(format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", current_user_sid()?))
}

fn is_protected(path: &Path) -> bool {
    let p = wide(&path.to_string_lossy());
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            p.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS || sd.is_null() {
        return false;
    }
    let (mut control, mut rev) = (0u16, 0u32);
    let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut rev) } != 0;
    unsafe { LocalFree(sd.cast()) };
    ok && control & SE_DACL_PROTECTED != 0
}

/// Apply an owner-only DACL to `path` (skipped when it already has a protected DACL, so the
/// check is cheap on every start).
pub fn restrict(path: &Path, dir: bool) -> io::Result<()> {
    if is_protected(path) {
        return Ok(());
    }
    let sddl = wide(&owner_only_sddl(dir)?);
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = null_mut();
    let ok = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } != 0;
    let rc = if ok {
        let p = wide(&path.to_string_lossy());
        unsafe {
            SetNamedSecurityInfoW(
                p.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null(),
            )
        }
    } else {
        1
    };
    unsafe { LocalFree(sd.cast()) };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricts_a_scratch_dir() {
        let d = std::env::temp_dir().join(format!("xssh-winsec-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(!is_protected(&d));
        restrict(&d, true).unwrap();
        assert!(is_protected(&d));
        // Files created inside inherit the owner-only DACL and stay usable.
        std::fs::write(d.join("f"), "x").unwrap();
        assert_eq!(std::fs::read_to_string(d.join("f")).unwrap(), "x");
        assert!(current_user_sid().unwrap().starts_with("S-1-"));
        assert_eq!(process_user_sid(std::process::id()).unwrap(), current_user_sid().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
