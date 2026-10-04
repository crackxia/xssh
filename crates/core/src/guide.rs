//! The agent manual (`xssh guide`) and the `SKILL.md` built from it for agent CLIs.

use std::path::Path;

pub const GUIDE: &str = include_str!("../../../docs/guide.md");

/// Skill directory name (`<agent skills dir>/xssh/SKILL.md`).
pub const SKILL_NAME: &str = "xssh";

/// What agents match tasks against: says when to use xssh, and that it replaces plain ssh.
const SKILL_DESCRIPTION: &str = "Use for ANY work on a remote server, host, VPS or IP (commands, sudo, deploys, files, logs, SSH) instead of ssh/scp/sftp/rsync. The user's servers are saved (`xssh host list`), no address or password needed. Covers one-off and parallel commands, interactive sessions with prompts, REPLs and full-screen apps (vim, remote Claude Code), long jobs, remote file read/edit, rsync-like copy local<->server<->server, port forwards and status/logs.";

/// `SKILL.md` content. `exe` is the xssh binary; naming it lets an agent run xssh even when its
/// folder is not on PATH (a portable install, or PATH not yet seen by an agent started earlier).
pub fn skill_md(exe: Option<&Path>) -> String {
    let mut s = format!("---\nname: {SKILL_NAME}\ndescription: {SKILL_DESCRIPTION}\n---\n\n");
    if let Some(exe) = exe {
        let native = exe.display().to_string();
        s.push_str(&format!("Executable: `{native}`"));
        // Git Bash eats unquoted backslashes; forward slashes work in every Windows shell.
        if native.contains('\\') {
            s.push_str(&format!(" (Git Bash: `{}`)", native.replace('\\', "/")));
        }
        s.push_str("; use it where `xssh` is not on PATH.\n\n");
    }
    s.push_str(GUIDE);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_frontmatter_and_executable() {
        let s = skill_md(Some(Path::new(r"D:\Tools\xssh\xssh.exe")));
        assert!(s.starts_with("---\nname: xssh\ndescription: "));
        // Agent CLIs cap the description (Claude Code: 1024 chars) and parse it as one YAML line.
        assert!(SKILL_DESCRIPTION.len() <= 1024 && !SKILL_DESCRIPTION.contains('\n') && !SKILL_DESCRIPTION.contains(": "));
        assert!(s.contains(r"Executable: `D:\Tools\xssh\xssh.exe` (Git Bash: `D:/Tools/xssh/xssh.exe`); use it"));
        assert!(!skill_md(Some(Path::new("/opt/xssh/xssh"))).contains("(Git Bash:"));
        assert!(!skill_md(None).contains("Executable"));
    }
}
