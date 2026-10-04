//! Desktop dialogs for the human steps (password entry, host key trust) when xssh runs without a
//! terminal, e.g. Claude Code's `!` commands. `None`: no dialog is available here.

use zeroize::Zeroizing;

/// Ask a yes/no question; the default answer is no.
pub fn confirm(title: &str, text: &str) -> Option<bool> {
    imp::confirm(title, text)
}

/// Ask for a secret. `Some(None)`: the user cancelled.
pub fn secret(title: &str, prompt: &str) -> Option<Option<Zeroizing<String>>> {
    imp::secret(title, prompt)
}

#[cfg(windows)]
mod imp {
    use super::Zeroizing;
    use std::ptr::null_mut;
    use windows_sys::Win32::Security::Credentials::{
        CREDUI_FLAGS_ALWAYS_SHOW_UI, CREDUI_FLAGS_DO_NOT_PERSIST, CREDUI_FLAGS_GENERIC_CREDENTIALS, CREDUI_FLAGS_KEEP_USERNAME,
        CREDUI_INFOW, CredUIPromptForCredentialsW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn confirm(title: &str, text: &str) -> Option<bool> {
        let (t, c) = (wide(text), wide(title));
        let r = unsafe {
            MessageBoxW(
                null_mut(),
                t.as_ptr(),
                c.as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
        (r != 0).then_some(r == IDYES)
    }

    pub fn secret(title: &str, prompt: &str) -> Option<Option<Zeroizing<String>>> {
        let (msg, cap, target) = (wide(prompt), wide(title), wide("xssh"));
        let info = CREDUI_INFOW {
            cbSize: size_of::<CREDUI_INFOW>() as u32,
            hwndParent: null_mut(),
            pszMessageText: msg.as_ptr(),
            pszCaptionText: cap.as_ptr(),
            hbmBanner: null_mut(),
        };
        // The user name field is fixed ("xssh"); only the password is entered.
        let mut user = [0u16; 64];
        for (d, s) in user.iter_mut().zip("xssh".encode_utf16()) {
            *d = s;
        }
        let mut pw = Zeroizing::new([0u16; 512]);
        let mut save = 0;
        let r = unsafe {
            CredUIPromptForCredentialsW(
                &info,
                target.as_ptr(),
                std::ptr::null(),
                0,
                user.as_mut_ptr(),
                user.len() as u32,
                pw.as_mut_ptr(),
                pw.len() as u32,
                &mut save,
                CREDUI_FLAGS_GENERIC_CREDENTIALS | CREDUI_FLAGS_ALWAYS_SHOW_UI | CREDUI_FLAGS_DO_NOT_PERSIST | CREDUI_FLAGS_KEEP_USERNAME,
            )
        };
        const ERROR_CANCELLED: u32 = 1223;
        match r {
            0 => {
                let n = pw.iter().position(|&c| c == 0).unwrap_or(pw.len());
                Some(Some(Zeroizing::new(String::from_utf16_lossy(&pw[..n]))))
            }
            ERROR_CANCELLED => Some(None),
            _ => None,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Zeroizing;
    use std::process::{Command, Stdio};

    fn has(cmd: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
    }

    /// AppleScript string literal.
    fn asq(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn linux_gui() -> bool {
        (std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()) && has("zenity")
    }

    pub fn confirm(title: &str, text: &str) -> Option<bool> {
        let out = if cfg!(target_os = "macos") {
            let script = format!(
                "display dialog {} with title {} buttons {{\"No\", \"Yes\"}} default button \"No\" with icon caution",
                asq(text),
                asq(title)
            );
            Command::new("osascript")
                .args(["-e", &script])
                .stderr(Stdio::null())
                .output()
                .ok()?
        } else if linux_gui() {
            Command::new("zenity")
                .args(["--question", "--default-cancel", "--title", title, "--text", text])
                .stderr(Stdio::null())
                .output()
                .ok()?
        } else {
            return None;
        };
        Some(out.status.success() && (!cfg!(target_os = "macos") || String::from_utf8_lossy(&out.stdout).contains("Yes")))
    }

    pub fn secret(title: &str, prompt: &str) -> Option<Option<Zeroizing<String>>> {
        let out = if cfg!(target_os = "macos") {
            let script = format!(
                "text returned of (display dialog {} with title {} default answer \"\" with hidden answer)",
                asq(prompt),
                asq(title)
            );
            Command::new("osascript")
                .args(["-e", &script])
                .stderr(Stdio::null())
                .output()
                .ok()?
        } else if linux_gui() {
            Command::new("zenity")
                .args(["--password", "--title", &format!("{title}: {prompt}")])
                .stderr(Stdio::null())
                .output()
                .ok()?
        } else {
            return None;
        };
        if !out.status.success() {
            return Some(None);
        }
        let s = Zeroizing::new(String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']).to_string());
        Some(Some(s))
    }
}
