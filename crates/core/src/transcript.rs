//! Session transcripts (`sessions/<id>[-<name>].log` in the xssh home): written by the engine,
//! read back by `session log` and the desktop app.

use crate::paths::Paths;
use std::path::PathBuf;

/// Starts a transcript annotation line (inputs, screens, TUI start/end); `render` interprets them.
pub const NOTE: char = '\u{E00F}';

/// The newest transcript of a closed session, by id or name (`<id>.log` / `<id>-<name>.log`).
pub fn find_closed(paths: &Paths, id: &str) -> Option<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(paths.sessions_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            let stem = n.strip_suffix(".log").unwrap_or("");
            stem == id || stem.starts_with(&format!("{id}-")) || stem.split_once('-').is_some_and(|(_, name)| name == id)
        })
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
        .collect();
    found.sort();
    found.pop().map(|(_, p)| p)
}

/// Readable transcript: the `session run` wrapper shown as the plain command, `[exit N]` after
/// each run, `[input] ...` for what was sent, and for full-screen programs the screens that were
/// returned instead of their raw redraw bytes. Trailing spaces trimmed, repeats collapsed.
pub fn render(content: &str) -> Vec<String> {
    static WRAP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#" printf '%s_%s\\n' __XSSH_B \w+; eval ('(?:[^']|'\\'')*'|\S+); printf '\\n%s_%s_%s\\n' __XSSH_E \w+ "\$\?""#)
            .unwrap()
    });
    static END: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"__XSSH_E_\w+_(\d+)").unwrap());
    let clean = crate::text::normalize_terminal(content);
    // Transcripts with `run` notes show the command from the note; older ones only have the
    // echoed wrapper, rewritten here to the plain command.
    let noted = content.contains(&format!("{NOTE}run "));
    let clean = WRAP.replace_all(&clean, |c: &regex::Captures| {
        if noted {
            return c[0].to_string();
        }
        let q = &c[1];
        match q.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
            Some(inner) => inner.replace(r"'\''", "'"),
            None => q.to_string(),
        }
    });
    let mut out: Vec<String> = vec![];
    let (mut in_tui, mut in_screen) = (false, false);
    for line in clean.lines() {
        let line = line.trim_end();
        if let Some(note) = line.strip_prefix(NOTE) {
            match note {
                "tui-begin" => {
                    in_tui = true;
                    out.push("[full-screen program started; screens below are what was returned]".into());
                }
                "tui-end" => {
                    in_tui = false;
                    out.push("[full-screen program ended]".into());
                }
                "screen" => {
                    in_screen = true;
                    out.push("--- screen ---".into());
                }
                "screen-end" => {
                    in_screen = false;
                    out.push("--- end screen ---".into());
                }
                n => out.push(if let Some(i) = n.strip_prefix("input ") {
                    format!("[input] {i}")
                } else if let Some(c) = n.strip_prefix("run ") {
                    // The command of a `session run` (its typed wrapper line is hidden below).
                    format!("$ {c}")
                } else {
                    format!("[{n}]")
                }),
            }
            continue;
        }
        if let Some(c) = END.captures(line) {
            out.push(format!("[exit {}]", &c[1]));
            continue;
        }
        // The typed `session run` wrapper (echoed, possibly wrapped over several lines) and the
        // prompt-hook setup line are plumbing, not output.
        if (in_tui && !in_screen)
            || line.contains("__XSSH_B")
            || line.contains("__XSSH_E ")
            || line.contains("__xssh_p")
            || line.contains("6973;P")
        {
            continue;
        }
        out.push(line.to_string());
    }
    // Collapse blank runs and 3+ identical lines.
    let mut res: Vec<String> = vec![];
    let mut i = 0;
    while i < out.len() {
        let mut j = i;
        while j + 1 < out.len() && out[j + 1] == out[i] {
            j += 1;
        }
        let n = j - i + 1;
        if out[i].is_empty() {
            if res.last().is_some_and(|l| !l.is_empty()) {
                res.push(String::new());
            }
        } else if n >= 3 {
            res.push(out[i].clone());
            res.push(format!("[x {} identical lines]", n - 1));
        } else {
            res.extend(out[i..=j].iter().cloned());
        }
        i = j + 1;
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_run_notes_and_hides_plumbing() {
        let n = NOTE;
        let t = format!(
            " if [ -n \"${{BASH_VERSION-}}\" ]; then eval '__xssh_p(){{ ...'; fi\r\n\
             $ \r\n{n}run ls -la\r\n\
             a@h:~$  printf '%s_%s\\n' __XSSH_B abcdefgh; eval 'ls -la'; printf '\\n%s_%s_%s\\n' __XSSH_E abcdefgh \"$?\"\r\n\
             __XSSH_B_abcdefgh\r\nfile1\r\n\r\n__XSSH_E_abcdefgh_0\r\na@h:~$ "
        );
        let lines = render(&t);
        assert_eq!(lines, vec!["$", "$ ls -la", "file1", "", "[exit 0]", "a@h:~$"], "{lines:?}");
        // Old transcripts (no notes): the wrapper becomes the plain command.
        let old = "a@h:~$  printf '%s_%s\\n' __XSSH_B abcdefgh; eval 'ls'; printf '\\n%s_%s_%s\\n' __XSSH_E abcdefgh \"$?\"\r\nx\r\n";
        assert_eq!(render(old)[0], "a@h:~$ ls");
    }
}
