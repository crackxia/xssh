//! Put the xssh folder on the user's PATH, so terminals and agent CLIs can run `xssh` by name.
//!
//! - Windows: the per-user `Path` value in `HKCU\Environment` (no admin rights needed), then a
//!   `WM_SETTINGCHANGE` broadcast so Explorer passes it to programs started afterwards.
//! - Unix: a marked block in the login-shell profiles (`~/.profile`, plus `~/.bash_profile` /
//!   `~/.zprofile` where those shells would skip `~/.profile`).
//!
//! The folder goes first, so `xssh` finds this build rather than an older copy further along.
//! Programs that are already running (terminals, agent sessions) keep their old PATH.

use std::path::{Path, PathBuf};
use xssh_core::error::Result;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathStatus {
    /// Registered by the user-level PATH setting this module manages.
    pub user: bool,
    /// Already reachable some other way (the system PATH on Windows).
    pub system: bool,
    /// The `xssh` that a newly started terminal or agent runs: the first one along its PATH.
    pub resolved: Option<PathBuf>,
    /// A PATH entry whose `"` is never closed (e.g. `C:\Program Files\X"`): cmd.exe then reads
    /// every later entry, the user PATH included, as one quoted folder that does not exist.
    pub unclosed_quote: Option<String>,
}

impl PathStatus {
    /// Whether `xssh` in a new terminal is the one in `dir` (not missing, not another copy).
    pub fn effective(&self, dir: &Path) -> bool {
        self.resolved
            .as_deref()
            .and_then(Path::parent)
            .is_some_and(|p| same_dir(&p.to_string_lossy(), dir))
    }
}

/// Whether a `;`/`:`-separated PATH value contains `dir`.
pub fn contains(value: &str, dir: &Path) -> bool {
    value.split(SEP).any(|e| same_dir(e, dir))
}

/// `value` with `dir` as its first entry (moved there when it appears further along).
pub fn with_dir(value: &str, dir: &Path) -> String {
    let rest = without_dir(value, dir);
    if rest.is_empty() {
        dir.display().to_string()
    } else {
        format!("{}{SEP}{rest}", dir.display())
    }
}

/// The first `xssh` executable in the absolute folders of `entries`, in PATH order.
fn find_exe<S: AsRef<str>>(entries: impl IntoIterator<Item = S>) -> Option<PathBuf> {
    let name = format!("xssh{}", std::env::consts::EXE_SUFFIX);
    entries.into_iter().find_map(|e| {
        let dir = Path::new(e.as_ref().trim().trim_matches('"'));
        let exe = dir.join(&name);
        (dir.is_absolute() && exe.is_file()).then_some(exe)
    })
}

/// PATH entries as cmd.exe splits them: `"` toggles quoting and a `;` inside quotes is part of
/// the entry; the quotes themselves are dropped.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_entries(value: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// The entry that opens a `"` which is never closed, if any.
#[cfg_attr(not(windows), allow(dead_code))]
fn unclosed_quote(value: &str) -> Option<String> {
    let mut open = None;
    for e in value.split(';').filter(|e| e.matches('"').count() % 2 == 1) {
        open = if open.is_some() { None } else { Some(e.to_string()) };
    }
    open
}

/// `value` without any entry naming `dir` (and without empty entries).
pub fn without_dir(value: &str, dir: &Path) -> String {
    value
        .split(SEP)
        .filter(|e| !e.is_empty() && !same_dir(e, dir))
        .collect::<Vec<_>>()
        .join(&SEP.to_string())
}

#[cfg(windows)]
const SEP: char = ';';
#[cfg(not(windows))]
const SEP: char = ':';

fn same_dir(entry: &str, dir: &Path) -> bool {
    let norm = |s: &str| {
        let s = s.trim().trim_matches('"').trim_end_matches(['\\', '/']).to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    let e = norm(entry);
    !e.is_empty() && e == norm(&dir.to_string_lossy())
}

pub use imp::{register, status, unregister};

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
    use xssh_core::error::Error;

    const USER_ENV: &str = "Environment";
    const SYSTEM_ENV: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

    /// Reads both registry values the way Explorer combines them for new programs: the system
    /// PATH, then the user's, with `%VAR%` expanded in `REG_EXPAND_SZ` values. `resolved` follows
    /// cmd.exe's quote-aware parsing, the strictest of the common shells.
    pub fn status(dir: &Path) -> Result<PathStatus> {
        let user = read(HKEY_CURRENT_USER, USER_ENV)?.map(expanded).unwrap_or_default();
        let system = read(HKEY_LOCAL_MACHINE, SYSTEM_ENV)
            .ok()
            .flatten()
            .map(expanded)
            .unwrap_or_default();
        let combined = format!("{system};{user}");
        Ok(PathStatus {
            user: contains(&user, dir),
            system: contains(&system, dir),
            resolved: find_exe(cmd_entries(&combined)),
            unclosed_quote: unclosed_quote(&combined),
        })
    }

    fn expanded((value, kind): (String, u32)) -> String {
        if kind != REG_EXPAND_SZ || !value.contains('%') {
            return value;
        }
        let src = wide(&value);
        let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), null_mut(), 0) };
        let mut buf = vec![0u16; n as usize];
        let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), n) };
        if n == 0 || n as usize > buf.len() {
            return value;
        }
        buf.truncate(n as usize - 1);
        String::from_utf16_lossy(&buf)
    }

    pub fn register(dir: &Path) -> Result<()> {
        update(HKEY_CURRENT_USER, USER_ENV, |v| with_dir(v, dir))?;
        broadcast();
        Ok(())
    }

    pub fn unregister(dir: &Path) -> Result<()> {
        update(HKEY_CURRENT_USER, USER_ENV, |v| without_dir(v, dir))?;
        broadcast();
        Ok(())
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(root: HKEY, subkey: &str, access: u32) -> Result<Key> {
        let mut key: HKEY = null_mut();
        let rc = unsafe { RegOpenKeyExW(root, wide(subkey).as_ptr(), 0, access, &mut key) };
        if rc != ERROR_SUCCESS {
            return Err(Error::io(format!("open registry key {subkey}: error {rc}")));
        }
        Ok(Key(key))
    }

    /// The `Path` value and its registry type, or None when it does not exist.
    pub(super) fn read(root: HKEY, subkey: &str) -> Result<Option<(String, u32)>> {
        let key = open(root, subkey, KEY_READ)?;
        read_value(&key)
    }

    fn read_value(key: &Key) -> Result<Option<(String, u32)>> {
        let name = wide("Path");
        let (mut kind, mut size) = (0u32, 0u32);
        let rc = unsafe { RegQueryValueExW(key.0, name.as_ptr(), null(), &mut kind, null_mut(), &mut size) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS {
            return Err(Error::io(format!("read PATH from the registry: error {rc}")));
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let rc = unsafe { RegQueryValueExW(key.0, name.as_ptr(), null(), &mut kind, buf.as_mut_ptr().cast(), &mut size) };
        if rc != ERROR_SUCCESS {
            return Err(Error::io(format!("read PATH from the registry: error {rc}")));
        }
        buf.truncate((size as usize) / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(Some((String::from_utf16_lossy(&buf), kind)))
    }

    /// Rewrite `Path`, keeping its type (`REG_EXPAND_SZ` keeps `%VAR%` entries working).
    pub(super) fn update(root: HKEY, subkey: &str, f: impl FnOnce(&str) -> String) -> Result<()> {
        let key = open(root, subkey, KEY_READ | KEY_SET_VALUE)?;
        let (old, kind) = read_value(&key)?.unwrap_or((String::new(), REG_EXPAND_SZ));
        let new = f(&old);
        if new == old {
            return Ok(());
        }
        let kind = if kind == REG_SZ { REG_SZ } else { REG_EXPAND_SZ };
        let data = wide(&new);
        let rc = unsafe { RegSetValueExW(key.0, wide("Path").as_ptr(), 0, kind, data.as_ptr().cast(), (data.len() * 2) as u32) };
        if rc != ERROR_SUCCESS {
            return Err(Error::io(format!("write PATH to the registry: error {rc}")));
        }
        Ok(())
    }

    /// Tell Explorer (and other top-level windows) that the environment changed.
    fn broadcast() {
        let env = wide("Environment");
        let mut result = 0usize;
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                env.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                3000,
                &mut result,
            )
        };
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Registry::{KEY_ALL_ACCESS, REG_OPTION_VOLATILE, RegCreateKeyExW, RegDeleteTreeW};

        /// Exercises the registry code on a throwaway volatile key (never the real PATH).
        /// Run with `cargo test -p xssh-store -- --ignored`.
        #[test]
        #[ignore]
        fn registry_roundtrip_on_scratch_key() {
            let sub = format!(r"Software\xssh-selftest-{}", std::process::id());
            let mut key: HKEY = null_mut();
            let rc = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&sub).as_ptr(),
                    0,
                    null(),
                    REG_OPTION_VOLATILE,
                    KEY_ALL_ACCESS,
                    null(),
                    &mut key,
                    null_mut(),
                )
            };
            assert_eq!(rc, ERROR_SUCCESS);
            drop(Key(key));
            let dir = Path::new(r"D:\Tools\xssh");
            let run = || -> Result<()> {
                assert_eq!(read(HKEY_CURRENT_USER, &sub)?, None);
                update(HKEY_CURRENT_USER, &sub, |v| with_dir(v, dir))?;
                assert_eq!(read(HKEY_CURRENT_USER, &sub)?, Some((r"D:\Tools\xssh".into(), REG_EXPAND_SZ)));
                update(HKEY_CURRENT_USER, &sub, |_| r"%USERPROFILE%\bin;C:\x".into())?;
                update(HKEY_CURRENT_USER, &sub, |v| with_dir(v, dir))?;
                let (v, kind) = read(HKEY_CURRENT_USER, &sub)?.unwrap();
                assert_eq!(v, r"D:\Tools\xssh;%USERPROFILE%\bin;C:\x");
                assert_eq!(kind, REG_EXPAND_SZ);
                update(HKEY_CURRENT_USER, &sub, |v| without_dir(v, dir))?;
                assert_eq!(read(HKEY_CURRENT_USER, &sub)?.unwrap().0, r"%USERPROFILE%\bin;C:\x");
                Ok(())
            };
            let r = run();
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&sub).as_ptr()) };
            unsafe { windows_sys::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, wide(&sub).as_ptr()) };
            r.unwrap();
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use xssh_core::error::Error;

    const BEGIN: &str = "# >>> xssh >>>";
    const END: &str = "# <<< xssh <<<";

    fn home() -> Result<PathBuf> {
        std::env::home_dir().ok_or_else(|| Error::io("cannot determine the user's home directory"))
    }

    /// Profiles a login shell reads: bash skips ~/.profile when ~/.bash_profile exists, zsh
    /// never reads it.
    fn targets(home: &Path) -> Vec<PathBuf> {
        let mut v = vec![home.join(".profile")];
        if home.join(".bash_profile").exists() {
            v.push(home.join(".bash_profile"));
        }
        let zsh = std::env::var("SHELL").is_ok_and(|s| s.ends_with("zsh"));
        if zsh || cfg!(target_os = "macos") || home.join(".zprofile").exists() {
            v.push(home.join(".zprofile"));
        }
        v
    }

    fn all_profiles(home: &Path) -> Vec<PathBuf> {
        [".profile", ".bash_profile", ".zprofile", ".bashrc", ".zshrc"]
            .iter()
            .map(|f| home.join(f))
            .collect()
    }

    pub fn status(dir: &Path) -> Result<PathStatus> {
        let home = home()?;
        let user = all_profiles(&home)
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .any(|t| block_dir(&t).is_some_and(|d| same_dir(&d, dir)));
        let path = std::env::var("PATH").unwrap_or_default();
        let system = !user && contains(&path, dir);
        // A login shell puts the block's folder in front of the inherited PATH.
        let first = user.then(|| dir.to_string_lossy().into_owned());
        let resolved = find_exe(first.into_iter().chain(path.split(SEP).map(String::from)));
        Ok(PathStatus {
            user,
            system,
            resolved,
            unclosed_quote: None,
        })
    }

    pub fn register(dir: &Path) -> Result<()> {
        let home = home()?;
        for f in targets(&home) {
            let old = std::fs::read_to_string(&f).unwrap_or_default();
            write(&f, &add_block(&old, dir))?;
        }
        Ok(())
    }

    pub fn unregister(_dir: &Path) -> Result<()> {
        let home = home()?;
        for f in all_profiles(&home) {
            if let Ok(old) = std::fs::read_to_string(&f) {
                let new = strip_block(&old);
                if new != old {
                    write(&f, &new)?;
                }
            }
        }
        Ok(())
    }

    fn write(f: &Path, text: &str) -> Result<()> {
        std::fs::write(f, text).map_err(|e| Error::io(format!("write {}: {e}", f.display())))
    }

    /// Text with the xssh block removed.
    pub(super) fn strip_block(text: &str) -> String {
        let mut out = String::new();
        let mut inside = false;
        for line in text.split_inclusive('\n') {
            let t = line.trim_end();
            if t == BEGIN {
                inside = true;
            } else if t == END && inside {
                inside = false;
            } else if !inside {
                out.push_str(line);
            }
        }
        out
    }

    /// Text with a (single, fresh) xssh block appended.
    pub(super) fn add_block(text: &str, dir: &Path) -> String {
        let mut out = strip_block(text);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let quoted: String = dir
            .to_string_lossy()
            .chars()
            .flat_map(|c| match c {
                '"' | '\\' | '$' | '`' => vec!['\\', c],
                c => vec![c],
            })
            .collect();
        out.push_str(&format!("{BEGIN}\nexport PATH=\"{quoted}:$PATH\"\n{END}\n"));
        out
    }

    /// The folder named in an existing xssh block.
    pub(super) fn block_dir(text: &str) -> Option<String> {
        let start = text.find(BEGIN)?;
        let line = text[start..].lines().nth(1)?;
        let v = line.strip_prefix("export PATH=\"")?.strip_suffix(":$PATH\"")?;
        let mut out = String::new();
        let mut chars = v.chars();
        while let Some(c) = chars.next() {
            out.push(if c == '\\' { chars.next().unwrap_or('\\') } else { c });
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn profile_block() {
            let dir = Path::new("/opt/my $tools/xssh");
            let t = add_block("export A=1", dir);
            assert!(t.starts_with("export A=1\n# >>> xssh >>>\n"));
            assert_eq!(block_dir(&t).as_deref(), Some("/opt/my $tools/xssh"));
            assert_eq!(add_block(&t, dir), t, "re-adding keeps a single block");
            assert_eq!(strip_block(&t), "export A=1\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn path_value_edits() {
        let d = Path::new(r"D:\Tools\xssh");
        assert_eq!(with_dir("", d), r"D:\Tools\xssh");
        assert_eq!(with_dir(r"C:\a;", d), r"D:\Tools\xssh;C:\a");
        assert!(contains(r"C:\a;d:\tools\XSSH\", d));
        assert_eq!(with_dir(r"C:\a;d:\tools\xssh\", d), r"D:\Tools\xssh;C:\a", "moved to the front");
        assert_eq!(with_dir(r"D:\Tools\xssh;C:\a", d), r"D:\Tools\xssh;C:\a");
        assert_eq!(without_dir(r"C:\a;D:\Tools\xssh;;%X%\b;d:\tools\xssh\", d), r"C:\a;%X%\b");
        assert!(!contains(r"D:\Tools\xssh2", d));
    }

    #[test]
    fn resolves_first_xssh_along_path() {
        let root = std::env::temp_dir().join(format!("xssh-path-test-{}", std::process::id()));
        let (old, new, empty) = (root.join("old"), root.join("new"), root.join("empty"));
        let exe = format!("xssh{}", std::env::consts::EXE_SUFFIX);
        for d in [&old, &new, &empty] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(old.join(&exe), "").unwrap();
        std::fs::write(new.join(&exe), "").unwrap();
        let s = |v: &[&Path]| v.iter().map(|p| p.display().to_string()).collect::<Vec<_>>();

        let shadowed = PathStatus {
            resolved: find_exe(s(&[&empty, &old, &new])),
            ..Default::default()
        };
        assert_eq!(shadowed.resolved, Some(old.join(&exe)));
        assert!(!shadowed.effective(&new), "an older copy earlier on PATH wins");
        let fixed = PathStatus {
            resolved: find_exe(s(&[&new, &old])),
            ..Default::default()
        };
        assert!(fixed.effective(&new));
        assert_eq!(find_exe(["relative", ""]), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cmd_quote_parsing() {
        // A stray `"` at the end of the system PATH swallows the whole user PATH in cmd.exe.
        let broken = r#"C:\Windows;C:\Program Files\PowerShell\7";D:\xssh;C:\tools"#;
        assert_eq!(
            cmd_entries(broken),
            [r"C:\Windows", r"C:\Program Files\PowerShell\7;D:\xssh;C:\tools"]
        );
        assert_eq!(unclosed_quote(broken).as_deref(), Some(r#"C:\Program Files\PowerShell\7""#));
        let ok = r#"C:\Windows;"C:\a;b";D:\xssh"#;
        assert_eq!(cmd_entries(ok), [r"C:\Windows", r"C:\a;b", r"D:\xssh"]);
        assert_eq!(unclosed_quote(ok), None);
        assert_eq!(unclosed_quote(r"C:\x;;D:\y"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn path_value_edits() {
        let d = Path::new("/opt/xssh");
        assert_eq!(with_dir("/usr/bin:/opt/xssh", d), "/opt/xssh:/usr/bin");
        assert!(contains("/usr/bin:/opt/xssh/", d));
        assert_eq!(without_dir("/opt/xssh:/usr/bin", d), "/usr/bin");
    }
}
