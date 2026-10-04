//! Persistent sessions: the shell runs inside a remote tmux session and the daemon attaches to it
//! in tmux control mode (`tmux -C attach`) over a plain exec channel. The shell and everything it
//! runs survive network drops and daemon restarts; attaching again restores the live screen.
//!
//! Control mode is a line protocol: pane output arrives as `%output %<pane> <data>` with bytes
//! below 0x20 and `\` written as octal escapes, and input is sent as `send-keys -H` commands.

use serde::{Deserialize, Serialize};
use xssh_core::error::Result;
use xssh_core::paths::Paths;

/// tmux session name for an xssh session (`xssh-<name or id>`).
pub fn session_name(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("xssh-{safe}")
}

/// Target for pane commands (`send-keys`, `display`, `capture-pane`): the exact session name
/// (`=`), its current window and pane (the `:`). A bare `=name` only works for session commands.
pub fn pane_target(name: &str) -> String {
    format!("={name}:")
}

/// A line of control-mode output.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Raw bytes written by the pane's program.
    Output(Vec<u8>),
    /// The control client ended (session killed, or its last shell exited).
    Exit,
    /// Anything else (`%begin`/`%end` blocks, layout changes, command replies).
    Other,
}

pub fn parse_line(line: &[u8]) -> Event {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if let Some(rest) = line.strip_prefix(b"%output ") {
        // `%output %<pane> <data>`
        let data = match rest.iter().position(|&b| b == b' ') {
            Some(i) => &rest[i + 1..],
            None => &[][..],
        };
        return Event::Output(unescape(data));
    }
    if line == b"%exit" || line.starts_with(b"%exit ") {
        return Event::Exit;
    }
    Event::Other
}

/// Undo control mode's `\ooo` octal escapes (and `\\`).
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\\' {
            let oct = data.get(i + 1..i + 4);
            if let Some(o) = oct
                && o.iter().all(|b| (b'0'..=b'7').contains(b))
            {
                out.push((o[0] - b'0') * 64 + (o[1] - b'0') * 8 + (o[2] - b'0'));
                i += 4;
                continue;
            }
            if data.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// Control-mode commands that type `bytes` into the session's pane (hex, so any byte is safe).
pub fn send_keys(target: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in bytes.chunks(200) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        out.extend_from_slice(format!("send-keys -t {target} -H {}\n", hex.join(" ")).as_bytes());
    }
    out
}

pub fn resize(cols: u16, rows: u16) -> Vec<u8> {
    format!("refresh-client -C {cols},{rows}\n").into_bytes()
}

/// Shell command that creates the tmux session (detached); it fails with "duplicate session" when
/// one of that name exists. `shell` replaces the login shell (for non-POSIX login shells).
pub fn create_cmd(name: &str, cols: u16, rows: u16, shell: Option<&str>) -> String {
    let mut c = format!("tmux new-session -d -s {name} -x {cols} -y {rows}");
    if let Some(sh) = shell {
        c.push_str(&format!(" {}", xssh_core::text::shq(sh)));
    }
    c
}

/// Shell command that detaches leftover control-mode clients from the session: one whose SSH
/// connection died does not always exit, and it would keep a tmux server that has lost its last
/// session from exiting (new clients then fail with "server exited unexpectedly"). People's own
/// (non-control) clients are left alone.
pub fn detach_stale_cmd(name: &str) -> String {
    format!(
        "tmux list-clients -t ={name} -F '#{{client_control_mode}} #{{client_pid}} #{{client_name}}' 2>/dev/null | \
         while read -r c p n; do [ \"$c\" = 1 ] && {{ tmux detach-client -t \"$n\"; kill -HUP \"$p\"; }} 2>/dev/null; done; true"
    )
}

/// Shell command that ends the tmux session (and any control client stuck on it).
pub fn kill_cmd(name: &str) -> String {
    format!("{}; tmux kill-session -t ={name}", detach_stale_cmd(name))
}

/// Shell command printing what a newly attached client needs to rebuild the screen: a status
/// line (`XSSH <cursor_x> <cursor_y> <alternate_on> <pane_dead>`), then the visible screen with
/// its colors.
pub fn snapshot_cmd(name: &str) -> String {
    let t = pane_target(name);
    format!("tmux display -p -t {t} 'XSSH #{{cursor_x}} #{{cursor_y}} #{{alternate_on}} #{{pane_dead}}' && tmux capture-pane -p -e -t {t}")
}

/// Bytes that make a fresh terminal model show the snapshot printed by [`snapshot_cmd`].
pub fn snapshot_to_terminal(out: &str) -> Option<Vec<u8>> {
    let (head, screen) = out.split_once('\n').unwrap_or((out, ""));
    let mut f = head.strip_prefix("XSSH ")?.split_whitespace();
    let x: u16 = f.next()?.parse().ok()?;
    let y: u16 = f.next()?.parse().ok()?;
    let alt = f.next() == Some("1");
    let mut bytes = Vec::new();
    if alt {
        bytes.extend_from_slice(b"\x1b[?1049h");
    }
    bytes.extend_from_slice(b"\x1b[H\x1b[2J");
    let lines: Vec<&str> = screen.trim_end_matches('\n').split('\n').collect();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(l.as_bytes());
    }
    bytes.extend_from_slice(format!("\x1b[0m\x1b[{};{}H", y + 1, x + 1).as_bytes());
    Some(bytes)
}

/// What is needed to find and reattach a persistent session after a daemon restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistRec {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub host: String,
    pub tmux: String,
    #[serde(default)]
    pub owner: String,
    pub created_at: String,
    pub transcript: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub no_autofill: bool,
}

impl PersistRec {
    pub fn key(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// How a registered session that no client is attached to shows up in `session list`.
    pub fn detached_info(&self) -> xssh_core::api::SessionInfo {
        xssh_core::api::SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            created_at: self.created_at.clone(),
            idle_secs: 0,
            state: xssh_core::api::SessionState::Disconnected,
            cols: self.cols,
            rows: self.rows,
            transcript: self.transcript.clone(),
            pending_run: None,
            last: String::new(),
            cmd: String::new(),
            prompt: None,
            owner: self.owner.clone(),
            persistent: true,
        }
    }
}

fn registry_file(paths: &Paths) -> std::path::PathBuf {
    paths.sessions_dir().join("persistent.json")
}

pub fn load(paths: &Paths) -> Vec<PersistRec> {
    std::fs::read_to_string(registry_file(paths))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(paths: &Paths, recs: &[PersistRec]) -> Result<()> {
    let data = serde_json::to_vec_pretty(recs)?;
    xssh_core::paths::write_private(&registry_file(paths), &data)
}

/// Add or replace (by id) a record.
pub fn upsert(paths: &Paths, rec: &PersistRec) -> Result<()> {
    let mut all = load(paths);
    all.retain(|r| r.id != rec.id && !(rec.name.is_some() && r.name == rec.name && r.host == rec.host));
    all.push(rec.clone());
    save(paths, &all)
}

pub fn remove(paths: &Paths, id: &str) -> Result<()> {
    let mut all = load(paths);
    let before = all.len();
    all.retain(|r| r.id != id);
    if all.len() != before { save(paths, &all) } else { Ok(()) }
}

pub fn find(paths: &Paths, key: &str) -> Option<PersistRec> {
    load(paths).into_iter().find(|r| r.id == key || r.name.as_deref() == Some(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_mode_lines() {
        assert_eq!(parse_line(b"%output %0 echo hi\\015\\012"), Event::Output(b"echo hi\r\n".to_vec()));
        assert_eq!(
            parse_line(b"%output %3 \\033[?2004h\\\\x \xe4\xb8\xad"),
            Event::Output(b"\x1b[?2004h\\x \xe4\xb8\xad".to_vec())
        );
        assert_eq!(parse_line(b"%exit"), Event::Exit);
        assert_eq!(parse_line(b"%exit detached"), Event::Exit);
        assert_eq!(parse_line(b"%begin 1 2 0"), Event::Other);
    }

    #[test]
    fn encodes_input_and_snapshots() {
        assert_eq!(
            send_keys(&pane_target("xssh-w"), b"ls\r"),
            b"send-keys -t =xssh-w: -H 6c 73 0d\n".to_vec()
        );
        assert_eq!(send_keys("t", &[b'a'; 250]).iter().filter(|&&b| b == b'\n').count(), 2);
        assert_eq!(session_name("my build/1"), "xssh-my_build_1");
        let t = snapshot_to_terminal("XSSH 2 1 0 0\nline1\n$ \n").unwrap();
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(&t);
        assert_eq!(p.screen().cursor_position(), (1, 2));
        assert!(p.screen().contents().starts_with("line1\n$"));
        assert!(snapshot_to_terminal("garbage").is_none());
    }
}
