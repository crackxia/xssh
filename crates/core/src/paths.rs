//! Filesystem layout of the xssh home directory.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    /// Resolve the home directory: `--home` / `XSSH_HOME`, else `data` next to the executable,
    /// so an install is a self-contained folder (the CLI and the desktop app share it when
    /// they sit side by side).
    pub fn resolve(explicit: Option<&Path>) -> Result<Self> {
        let home = match explicit {
            // `C:/a/b` from Git Bash would otherwise print as `C:/a/b\sessions\x.log`.
            Some(p) if cfg!(windows) => PathBuf::from(p.to_string_lossy().replace('/', "\\")),
            Some(p) => p.to_path_buf(),
            None => default_home()?,
        };
        let p = Paths { home };
        p.ensure()?;
        Ok(p)
    }

    fn ensure(&self) -> Result<()> {
        for d in [
            self.home.clone(),
            self.run_dir(),
            self.sessions_dir(),
            self.outputs_dir(),
            self.jobs_dir(),
            self.keys_dir(),
        ] {
            std::fs::create_dir_all(&d).map_err(|e| {
                Error::io(format!("create {}: {e}", d.display()))
                    .hint("put xssh in a writable folder, or choose the data directory with --home / XSSH_HOME")
            })?;
        }
        restrict_dir(&self.home);
        Ok(())
    }

    /// Random id of this home, created on first use. Namespaces the OS keyring so that two
    /// installs never share credentials; it moves with the folder.
    pub fn instance_id(&self) -> Result<String> {
        let f = self.home.join("instance.id");
        if let Ok(s) = std::fs::read_to_string(&f) {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
        use rand::RngExt;
        let mut raw = [0u8; 6];
        rand::rng().fill(&mut raw);
        let id = hex::encode(raw);
        // Two processes may race on first use: create exclusively, then read back the winner.
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&f) {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(id.as_bytes())
                    .map_err(|e| Error::io(format!("write {}: {e}", f.display())))?;
                Ok(id)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(std::fs::read_to_string(&f)?.trim().to_string())
            }
            Err(e) => Err(Error::io(format!("create {}: {e}", f.display()))),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn hosts_file(&self) -> PathBuf {
        self.home.join("hosts.toml")
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.home.join("known_hosts")
    }
    pub fn audit_file(&self) -> PathBuf {
        self.home.join("audit.jsonl")
    }
    pub fn secrets_file(&self) -> PathBuf {
        self.home.join("secrets.enc")
    }
    pub fn master_key_file(&self) -> PathBuf {
        self.home.join("master.key")
    }
    pub fn secret_index_file(&self) -> PathBuf {
        self.home.join("secrets.index.json")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.run_dir().join("daemon.log")
    }
    pub fn daemon_token(&self) -> PathBuf {
        self.run_dir().join("daemon.token")
    }
    pub fn daemon_pid(&self) -> PathBuf {
        self.run_dir().join("daemon.pid")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.home.join("sessions")
    }
    pub fn outputs_dir(&self) -> PathBuf {
        self.home.join("outputs")
    }
    pub fn jobs_dir(&self) -> PathBuf {
        self.home.join("jobs")
    }
    pub fn keys_dir(&self) -> PathBuf {
        self.home.join("keys")
    }

    /// Name of the daemon's local socket. Derived from the home path so that
    /// different homes (e.g. tests) get independent daemons.
    pub fn socket_name(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.home.to_string_lossy().as_bytes());
        let tag = hex::encode(&digest[..6]);
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".into());
        let user: String = user.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        format!("xssh-{user}-{tag}")
    }

    /// Unix socket path (unused on Windows, where a named pipe is used).
    pub fn socket_path(&self) -> PathBuf {
        self.run_dir().join("daemon.sock")
    }
}

/// `data` next to the running executable (on Unix symlinks are resolved, so a linked binary keeps
/// its data beside the real file).
fn default_home() -> Result<PathBuf> {
    let exe = current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::io("cannot locate the xssh executable's folder; set XSSH_HOME"))?;
    Ok(dir.join("data"))
}

/// The running executable as a plain absolute path; on Unix with symlinks resolved.
///
/// Windows reports the path the program was started from, which is what we want: resolving it
/// would pick an arbitrary name of a hard-linked file (cargo hard-links `target/*/xssh.exe` to
/// `target/*/deps/xssh.exe`, and canonicalizing returned the `deps` one).
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| Error::io(format!("cannot locate the xssh executable: {e}")))?;
    #[cfg(not(windows))]
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    Ok(cargo_output(strip_verbatim(&exe)))
}

/// A cargo build started from `target/<profile>/deps/` (e.g. through a PATH entry that points
/// there) becomes the hard-linked `target/<profile>/<name>` next to it, so both names share one
/// data folder, and PATH entries and skills never point into `deps`. The Windows linker names
/// `xssh-desktop` as `deps/xssh_desktop.exe`.
fn cargo_output(exe: PathBuf) -> PathBuf {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) else {
        return exe;
    };
    if dir.file_name().is_none_or(|d| d != "deps") {
        return exe;
    }
    let Some(profile) = dir.parent() else { return exe };
    [name.to_string(), name.replace('_', "-")]
        .into_iter()
        .map(|n| profile.join(n))
        .find(|p| p.is_file())
        .unwrap_or(exe)
}

/// Verbatim `\\?\C:\...` paths (e.g. a program started through one) become plain paths for
/// display and child-process arguments.
#[cfg(windows)]
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}
#[cfg(not(windows))]
fn strip_verbatim(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// Write a file atomically (temp file + rename) with owner-only permissions.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data).map_err(|e| Error::io(format!("write {}: {e}", tmp.display())))?;
    restrict_file(&tmp);
    std::fs::rename(&tmp, path).map_err(|e| Error::io(format!("rename {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(unix)]
pub fn restrict_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
/// Owner-only DACL (current user, SYSTEM, Administrators); best effort (e.g. FAT volumes have
/// no ACLs).
#[cfg(windows)]
pub fn restrict_file(path: &Path) {
    let _ = crate::winsec::restrict(path, false);
}
#[cfg(not(any(unix, windows)))]
pub fn restrict_file(_path: &Path) {}

#[cfg(unix)]
fn restrict_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}
/// The home holds the daemon token, known_hosts, audit log and (file backend) secrets: without
/// this it would inherit the ACL of wherever xssh was unpacked (often writable by all users).
#[cfg(windows)]
fn restrict_dir(path: &Path) {
    let _ = crate::winsec::restrict(path, true);
}
#[cfg(not(any(unix, windows)))]
fn restrict_dir(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_deps_maps_to_profile_dir() {
        let root = std::env::temp_dir().join(format!("xssh-exe-test-{}", std::process::id()));
        let deps = root.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        for f in [
            "deps/xssh.exe",
            "deps/xssh_desktop.exe",
            "deps/lonely.exe",
            "xssh.exe",
            "xssh-desktop.exe",
        ] {
            std::fs::write(root.join(f), "").unwrap();
        }
        assert_eq!(cargo_output(deps.join("xssh.exe")), root.join("xssh.exe"));
        assert_eq!(cargo_output(deps.join("xssh_desktop.exe")), root.join("xssh-desktop.exe"));
        assert_eq!(cargo_output(deps.join("lonely.exe")), deps.join("lonely.exe"));
        assert_eq!(cargo_output(root.join("xssh.exe")), root.join("xssh.exe"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
