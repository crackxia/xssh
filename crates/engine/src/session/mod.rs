//! Persistent interactive PTY sessions held by the daemon.
//!
//! Output is tracked two ways: a cleaned text transcript with one read cursor per reader (agent)
//! for incremental `read`/`expect`/`run`, and a vt100 screen model (for TUI programs and prompt
//! detection).
//!
//! The shell prints an invisible marker before every prompt (see [`marker`]), so the session knows
//! when the shell is back at its prompt whatever the prompt looks like; the prompt regexes are
//! only a fallback (shells without the hook, nested shells) and for questions asked by programs.
//!
//! A session runs either on the SSH channel's PTY, or (`--persist`) inside a remote tmux session
//! reached through tmux control mode (see [`tmux`]), which survives disconnects.

pub mod keys;
pub mod marker;
pub mod prompt;
pub mod tmux;

use crate::ssh::Conn;
use prompt::PromptKind;
use regex::Regex;
use russh::client::Msg;
use russh::{ChannelMsg, ChannelWriteHalf};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::paths::Paths;
use xssh_core::text::{self, StreamCleaner, short_id, shq};
use xssh_store::config::ResponderRule;
use zeroize::Zeroizing;

/// Max transcript bytes kept in memory per session.
const TEXT_WINDOW: usize = 2 * 1024 * 1024;

pub use xssh_core::api::{OpenParams, ScreenSnapshot, SessionInfo, SessionState, WaitSpec};

/// Reader name used when a request does not say who is asking.
pub const DEFAULT_READER: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub session: String,
    /// New cleaned output since the previous read (ANSI stripped).
    pub output: String,
    pub state: SessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptKind>,
    /// The line the cursor is on (useful to see what is being asked).
    pub current_line: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// True when the command finished (`run` end marker, or the shell prompt back after a
    /// command typed with `send`), or the shell exited.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub completed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    /// A full-screen program is active (alternate screen or an inline TUI that redraws):
    /// `output` then holds only lines that scrolled off the top, and `screen` the live screen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alt_screen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
    /// The screen is identical to the one returned by the previous call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub screen_unchanged: bool,
    /// A busy indicator (e.g. "esc to interrupt") is visible: the program is still working.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
    /// The wait condition was not met before the timeout.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    /// A `session run` command is still unfinished (running or waiting for input).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unfinished: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_output_path: Option<String>,
}

struct Responder {
    re: Regex,
    reply: Zeroizing<String>,
    enter: bool,
    label: String,
}

/// Secrets resolved when the session is opened.
pub struct SessionSecrets {
    pub sudo_password: Option<Zeroizing<String>>,
    pub responders: Vec<(ResponderRule, Zeroizing<String>)>,
    pub redact: Vec<String>,
}

/// A `run` in flight. Markers are located as output arrives, so they survive
/// the transcript window being trimmed by huge outputs.
struct RunTrack {
    nonce: String,
    begin_marker: String,
    end_re: Regex,
    /// Absolute offset where the command's output starts (just after the begin marker line).
    begin: Option<u64>,
    /// Absolute offset of the end marker and the exit code it carried.
    end: Option<(u64, i64)>,
    /// The reader that started it: only its read consumes the result.
    owner: String,
}

/// Per-reader view of the session: each agent reads its own new output.
#[derive(Default)]
struct Reader {
    cursor: u64,
    /// Scrollback lines already reported, the newest of them (to find new ones once the
    /// scrollback is full), and the last screen returned.
    sb_seen: usize,
    sb_anchor: Vec<String>,
    last_screen_shown: Option<String>,
    /// Hint categories already shown to this reader (each is shown once).
    hinted: HashSet<&'static str>,
    /// Sequence number of the last event returned to this reader.
    events_seen: u64,
}

/// How the screen was redrawn by an output chunk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Redraw {
    None,
    /// Cursor-home / clear-screen: a program repainting the whole screen (top, dialogs).
    Full,
    /// Cursor-up combined with erase-line: an inline TUI (Ink apps such as Claude Code) or a
    /// multi-line progress display.
    Inline,
}

struct State {
    parser: vt100::Parser,
    cleaner: StreamCleaner,
    scanner: marker::Scanner,
    text: String,
    base: u64,
    readers: HashMap<String, Reader>,
    /// Hint categories shown once per session (not per reader).
    session_hints: HashSet<&'static str>,
    last_output: Instant,
    last_activity: Instant,
    closed: bool,
    /// The connection dropped (not the shell exiting).
    disconnected: bool,
    /// tmux backend: the control client ended while the tmux session lives on.
    detached: bool,
    tmux_exit: bool,
    exit_code: Option<u32>,
    after_sudo: bool,
    sudo_password: Option<Zeroizing<String>>,
    sudo_disabled: bool,
    responders: Vec<Responder>,
    last_fill: Option<(u64, String)>,
    /// New events, moved to `event_log` (numbered) when an observation is built.
    events: Vec<String>,
    event_log: VecDeque<(u64, String)>,
    event_seq: u64,
    /// Absolute offset where the current shell prompt starts (at the last prompt marker).
    prompt_at: Option<u64>,
    run: Option<RunTrack>,
    /// Runs replaced while unfinished (a nested shell took over): their end markers show up
    /// when that shell exits.
    stale_runs: Vec<String>,
    /// A prompt marker has been seen: the shell's prompt hook works.
    hooked: bool,
    /// A line was submitted (Enter) since the last prompt marker.
    submitted: bool,
    /// ctrl-c was sent while a `run` was unfinished (its end marker will never come).
    interrupted: bool,
    last_prompt_exit: Option<i64>,
    /// A program switched on interactive terminal modes (bracketed paste, focus or mouse
    /// reporting, keyboard protocols) since the last prompt: it reads keys, it is not a progress bar.
    interactive_modes: bool,
    /// Absolute offset of the last input sent (start point for `expect`).
    last_input_at: u64,
    /// Absolute offset just after the last `expect` match.
    last_expect_end: u64,
    redraws: VecDeque<(Instant, Redraw)>,
    /// An inline TUI was detected; stays set until the shell prompt is back.
    tui_sticky: bool,
    /// Signature of the screen with spinners/digits normalized, and when it last changed.
    screen_sig: u64,
    screen_changed_at: Instant,
    busy_patterns: Vec<Regex>,
    redact: Vec<String>,
    max_secret: usize,
    transcript: Option<std::fs::File>,
    transcript_written: u64,
    /// Full-screen program periods (absolute text offsets): their raw redraw bytes are noise,
    /// so they are left out of `output` (the agent saw the rendered screens instead).
    tui_active: bool,
    tui_began: u64,
    tui_ranges: VecDeque<(u64, u64)>,
    /// Last command started with `session run` (for `session list`).
    last_run: String,
}

pub use xssh_core::transcript::NOTE;

static STALE_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^__XSSH_[BE]_[a-z0-9]{8}(_\d+)?[ \t]*\n?").unwrap());
static ANY_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__XSSH_E_([a-z0-9]{8})_(\d+)\r?\n").unwrap());

impl State {
    fn new(rows: u16, cols: u16, encoding: Option<&str>) -> Self {
        State {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK),
            cleaner: StreamCleaner::new(encoding),
            scanner: marker::Scanner::default(),
            text: String::new(),
            base: 0,
            readers: HashMap::new(),
            session_hints: HashSet::new(),
            last_output: Instant::now(),
            last_activity: Instant::now(),
            closed: false,
            disconnected: false,
            detached: false,
            tmux_exit: false,
            exit_code: None,
            after_sudo: false,
            sudo_password: None,
            sudo_disabled: false,
            responders: vec![],
            last_fill: None,
            events: vec![],
            event_log: VecDeque::new(),
            event_seq: 0,
            prompt_at: None,
            run: None,
            stale_runs: vec![],
            hooked: false,
            submitted: false,
            interrupted: false,
            last_prompt_exit: None,
            interactive_modes: false,
            last_input_at: 0,
            last_expect_end: 0,
            redraws: VecDeque::new(),
            tui_sticky: false,
            screen_sig: 0,
            screen_changed_at: Instant::now(),
            busy_patterns: vec![],
            redact: vec![],
            max_secret: 0,
            transcript: None,
            transcript_written: 0,
            tui_active: false,
            tui_began: 0,
            tui_ranges: VecDeque::new(),
            last_run: String::new(),
        }
    }

    fn total(&self) -> u64 {
        self.base + self.text.len() as u64
    }

    fn idx(&self, abs: u64) -> usize {
        let mut i = (abs.max(self.base) - self.base) as usize;
        i = i.min(self.text.len());
        while i < self.text.len() && !self.text.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    /// The reader's state, created on first use at the start of the latest input's output.
    fn reader(&mut self, name: &str) -> &mut Reader {
        let start = self.last_input_at.max(self.base);
        let seq = self.event_seq;
        self.readers.entry(name.to_string()).or_insert_with(|| Reader {
            cursor: start,
            events_seen: seq,
            ..Default::default()
        })
    }

    /// Events this reader has not seen yet (every reader gets each event once).
    fn take_events(&mut self, reader: &str) -> Vec<String> {
        for e in std::mem::take(&mut self.events) {
            self.event_seq += 1;
            if self.event_log.len() >= 64 {
                self.event_log.pop_front();
            }
            self.event_log.push_back((self.event_seq, e));
        }
        let seen = self.reader(reader).events_seen;
        let out = self.event_log.iter().filter(|(n, _)| *n > seen).map(|(_, e)| e.clone()).collect();
        let seq = self.event_seq;
        self.reader(reader).events_seen = seq;
        out
    }

    /// A program asks for input: a password/confirmation prompt at once, a generic line ending
    /// in ':' or '?' only after a longer silence (it may be a progress label like "Stopping:").
    fn asks_input(&self, s: SessionState, k: Option<PromptKind>) -> bool {
        s == SessionState::WaitingInput
            && match k {
                Some(PromptKind::Input) => self.last_output.elapsed() >= Duration::from_millis(2500),
                Some(_) => true,
                None => false,
            }
    }

    /// The shell printed its prompt marker after the last submitted line.
    fn at_marker(&self) -> bool {
        self.hooked && !self.submitted
    }

    /// At a shell prompt by the marker, or (no working hook) by the prompt's look.
    fn at_shell_now(&self) -> bool {
        self.at_marker() || (!self.hooked && prompt::classify(&self.current_line()) == Some(PromptKind::Shell))
    }

    fn push_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(s);
        // Mask secrets in place with same-length '*' so absolute offsets stay valid; look back far
        // enough to catch a secret split across chunks.
        let from = floor_boundary(&self.text, start.saturating_sub(self.max_secret));
        for secret in self.redact.iter().filter(|s| s.len() >= 3) {
            let mut at = from;
            while let Some(p) = self.text[at..].find(secret.as_str()) {
                let a = at + p;
                self.text.replace_range(a..a + secret.len(), &"*".repeat(secret.len()));
                at = a + secret.len();
            }
        }
        self.track_run(floor_boundary(&self.text, start.saturating_sub(128)));
        self.flush_transcript(false);
        if self.text.len() > TEXT_WINDOW {
            let cut = floor_boundary(&self.text, self.text.len() - TEXT_WINDOW / 2);
            self.flush_transcript(true);
            self.text.drain(..cut);
            self.base += cut as u64;
        }
    }

    /// Locate the run markers in text from byte index `from`.
    fn track_run(&mut self, from: usize) {
        let base = self.base;
        if let Some(run) = self.run.as_mut()
            && run.begin.is_none()
            && let Some(p) = self.text[from..].find(&run.begin_marker)
        {
            let after = from + p + run.begin_marker.len();
            if let Some(nl) = self.text[after..].find('\n') {
                run.begin = Some(base + (after + nl + 1) as u64);
            }
        }
        // End markers of runs a nested shell took over: that shell has exited, and the command
        // now running in the outer shell (typically `exit`) is over with its status.
        if !self.stale_runs.is_empty() {
            let hits: Vec<(usize, String, i64)> = ANY_END
                .captures_iter(&self.text[from..])
                .map(|c| (from + c.get(0).unwrap().start(), c[1].to_string(), c[2].parse().unwrap_or(-1)))
                .collect();
            for (pos, nonce, code) in hits {
                let Some(i) = self.stale_runs.iter().position(|n| *n == nonce) else {
                    continue;
                };
                self.stale_runs.remove(i);
                if let Some(run) = self.run.as_mut()
                    && run.end.is_none()
                    && run.begin.is_some_and(|b| b <= base + pos as u64)
                {
                    run.end = Some((base + pos as u64, code));
                    self.events.push(format!(
                        "the nested shell exited (status {code}); back in the shell it was started from"
                    ));
                }
            }
        }
        let Some(run) = self.run.as_mut() else { return };
        if let (Some(begin), None) = (run.begin, run.end) {
            let scan = from.max((begin - base.min(begin)) as usize).min(self.text.len());
            let scan = floor_boundary(&self.text, scan);
            if let Some(c) = run.end_re.captures(&self.text[scan..]) {
                let m = c.get(0).unwrap();
                let code = c.get(1).and_then(|x| x.as_str().parse().ok()).unwrap_or(-1);
                run.end = Some((base + (scan + m.start()) as u64, code));
            }
        }
    }

    /// Feed raw bytes from the remote side; returns an auto-response to send, if any.
    fn ingest_bytes(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        // Decode first (remote encoding -> UTF-8), so the screen model and the transcript both
        // see correct characters.
        let utf8 = self.cleaner.decode(data);
        if let Some(e) = self.cleaner.switched
            && self.session_hints.insert("gbk")
        {
            self.events.push(format!(
                "some output was not UTF-8 and was decoded as {e}; if the host uses {e} throughout: `xssh host edit <host> --encoding {e}`"
            ));
        }
        for piece in self.scanner.feed(&utf8) {
            match piece {
                marker::Piece::Text(t) => self.ingest_text(&t),
                marker::Piece::Marker(code) => self.on_prompt_marker(code),
            }
        }
        self.last_output = Instant::now();
        autorespond(self)
    }

    fn ingest_text(&mut self, utf8: &str) {
        if !self.at_marker()
            && [
                "\x1b[?2004h",
                "\x1b[?1004h",
                "\x1b[?1000h",
                "\x1b[?1002h",
                "\x1b[?1003h",
                "\x1b[?1006h",
                "\x1b[>1u",
                "\x1b[=1u",
            ]
            .iter()
            .any(|m| utf8.contains(m))
        {
            self.interactive_modes = true;
        }
        // Split at an alternate-screen exit so the TUI period ends exactly there.
        let cut = ["\x1b[?1049l", "\x1b[?1047l", "\x1b[?47l"]
            .iter()
            .filter_map(|m| utf8.rfind(m).map(|i| i + m.len()))
            .max()
            .unwrap_or(0);
        for (i, part) in [&utf8[..cut], &utf8[cut..]].into_iter().enumerate() {
            if part.is_empty() {
                continue;
            }
            self.parser.process(part.as_bytes());
            self.update_screen_sig();
            self.update_tui(redraw_kind(part.as_bytes()));
            let cleaned = self.cleaner.strip(part);
            // The part ending in the alternate-screen exit still belongs to the program;
            // anything else starts/ends the period before it.
            if i == 0 {
                self.push_text(&cleaned);
                self.mark_tui();
            } else {
                self.mark_tui();
                self.push_text(&cleaned);
            }
        }
    }

    /// The shell is about to print its prompt.
    fn on_prompt_marker(&mut self, code: Option<i64>) {
        self.hooked = true;
        self.submitted = false;
        self.prompt_at = Some(self.total());
        self.last_prompt_exit = code;
        self.after_sudo = false;
        self.interactive_modes = false;
        // An interrupted run never prints its end marker: the prompt ends it.
        if self.interrupted {
            let at = self.total();
            if let Some(run) = self.run.as_mut()
                && run.end.is_none()
            {
                if run.begin.is_none() {
                    run.begin = Some(at);
                }
                run.end = Some((at, code.unwrap_or(130)));
                self.events.push("command interrupted; the shell prompt is back".into());
            }
            self.interrupted = false;
        }
        if self.tui_sticky && !self.parser.screen().alternate_screen() {
            self.tui_sticky = false;
            self.mark_tui();
        }
    }

    /// Write an annotation line to the transcript (after all output so far).
    fn note(&mut self, s: &str) {
        self.flush_transcript(true);
        if let Some(f) = self.transcript.as_mut() {
            let _ = f.write_all(format!("\r\n{NOTE}{s}\r\n").as_bytes());
        }
    }

    /// Track entering/leaving a full-screen program at the current text offset.
    fn mark_tui(&mut self) {
        let now = self.tui();
        if now == self.tui_active {
            return;
        }
        self.tui_active = now;
        let at = self.total();
        if now {
            self.tui_began = at;
            self.note("tui-begin");
        } else {
            if self.tui_ranges.len() >= 16 {
                self.tui_ranges.pop_front();
            }
            self.tui_ranges.push_back((self.tui_began, at));
            self.note("tui-end");
        }
    }

    /// `text[a..b]` without the parts written while a full-screen program was active.
    fn without_tui(&self, from: u64, to: u64) -> (String, bool) {
        let mut ranges: Vec<(u64, u64)> = self.tui_ranges.iter().copied().collect();
        if self.tui_active {
            ranges.push((self.tui_began, u64::MAX));
        }
        let mut out = String::new();
        let mut at = from;
        let mut cut = false;
        for (b, e) in ranges {
            if e <= at || b >= to {
                continue;
            }
            if b > at {
                out.push_str(&self.text[self.idx(at)..self.idx(b)]);
            }
            at = e.min(to);
            cut = true;
        }
        if at < to {
            out.push_str(&self.text[self.idx(at)..self.idx(to)]);
        }
        (out, cut)
    }

    /// Append masked text to the on-disk transcript, holding back a tail that could still
    /// turn out to be the start of a secret (unless `all`).
    fn flush_transcript(&mut self, all: bool) {
        let end = if all {
            self.total()
        } else {
            self.total().saturating_sub(self.max_secret as u64)
        };
        if end <= self.transcript_written {
            return;
        }
        let (a, b) = (self.idx(self.transcript_written), self.idx(end));
        if let Some(f) = self.transcript.as_mut() {
            let _ = f.write_all(&self.text.as_bytes()[a..b]);
        }
        self.transcript_written = self.base + b as u64;
    }

    fn current_line(&self) -> String {
        let screen = self.parser.screen();
        let (row, col) = screen.cursor_position();
        let line = screen.contents_between(row, 0, row, col);
        let line = if line.trim().is_empty() {
            // Cursor may sit after a wrapped prompt; fall back to the full row.
            screen.rows(0, screen.size().1).nth(row as usize).unwrap_or_default()
        } else {
            line
        };
        text::redact(&line, &self.redact)
    }

    fn state_now(&self, idle: Duration) -> (SessionState, Option<PromptKind>, String) {
        let line = self.current_line();
        if self.closed {
            let s = if self.disconnected {
                SessionState::Disconnected
            } else {
                SessionState::Exited
            };
            return (s, None, line);
        }
        if self.detached {
            return (SessionState::Disconnected, None, line);
        }
        let since = self.last_output.elapsed();
        if self.at_marker() && !self.parser.screen().alternate_screen() {
            let s = if since < idle.min(Duration::from_millis(150)) {
                SessionState::Running
            } else {
                SessionState::WaitingInput
            };
            return (s, Some(PromptKind::Shell), line);
        }
        let mut kind = prompt::classify(&line);
        // With a working hook, a prompt-shaped line without a marker is a nested shell (sudo -i,
        // su, ssh, docker exec) or output that merely looks like one: trust it after a quiet second.
        if kind == Some(PromptKind::Shell) && self.hooked && since < Duration::from_secs(1) {
            kind = None;
        }
        if since < idle || (kind.is_none() && since < Duration::from_secs(2)) {
            return (SessionState::Running, kind, line);
        }
        match kind {
            Some(k) => (SessionState::WaitingInput, Some(k), line),
            None => (SessionState::Quiet, None, line),
        }
    }

    /// A full-screen program is on screen: alternate screen, or a program like `top`
    /// that keeps redrawing the normal screen.
    fn tui(&self) -> bool {
        self.parser.screen().alternate_screen() || self.tui_sticky
    }

    /// Enter inline-TUI mode for programs that repaint the normal screen: repeated full
    /// repaints, or in-place redraws by a program that reads keys (interactive terminal modes
    /// on). Progress bars (cursor-up + erase, `\r` updates, a hidden cursor) do not count: their
    /// output stays in the command's output. Leave it when the shell prompt is back.
    fn update_tui(&mut self, redraw: Redraw) {
        if redraw != Redraw::None {
            if self.redraws.len() >= 8 {
                self.redraws.pop_front();
            }
            self.redraws.push_back((Instant::now(), redraw));
        }
        let at_shell = self.at_shell_now();
        if at_shell && !self.hooked {
            self.interactive_modes = false;
        }
        if self.tui_sticky {
            if at_shell && redraw == Redraw::None {
                self.tui_sticky = false;
            }
            return;
        }
        if at_shell || redraw == Redraw::None || self.parser.screen().alternate_screen() {
            return;
        }
        let recent = |full_only: bool| {
            self.redraws
                .iter()
                .filter(|(t, k)| t.elapsed() < Duration::from_secs(10) && (!full_only || *k == Redraw::Full))
                .count()
        };
        let s = self.parser.screen();
        let reads_keys = self.interactive_modes
            || s.bracketed_paste()
            || s.application_cursor()
            || s.mouse_protocol_mode() != vt100::MouseProtocolMode::None;
        if recent(true) >= 2 || (recent(false) >= 2 && reads_keys) {
            self.tui_sticky = true;
            // Report only what scrolls off from now on.
            let n = self.scrollback_len();
            for r in self.readers.values_mut() {
                r.sb_seen = n;
                r.sb_anchor.clear();
            }
        }
    }

    /// Take the reader's output since its cursor. While a `run` is active only its own output
    /// (between the markers) is returned. Returns (output, exit code, completed).
    fn take_output(&mut self, reader: &str) -> (String, Option<i64>, bool) {
        // What the agent is shown is final: the transcript may hold it too.
        self.flush_transcript(true);
        let total = self.total();
        let mut from = self.reader(reader).cursor;
        let mut to = total;
        let mut done = None;
        let mut owner = true;
        if let Some(run) = &self.run {
            match run.begin {
                // Only the echo of the wrapper so far.
                None => {
                    self.reader(reader).cursor = total;
                    return (String::new(), None, false);
                }
                Some(b) => from = from.max(b),
            }
            if let Some((end, code)) = run.end {
                to = end.max(from);
                done = Some(code);
                owner = run.owner == reader;
            }
        }
        // Back at the marked prompt: the prompt line itself is not output.
        if done.is_none()
            && self.at_marker()
            && let Some(p) = self.prompt_at
            && p >= from
            && p <= to
        {
            to = p;
        }
        let mut out = String::new();
        if from < self.base {
            out.push_str(&format!(
                "[xssh: {} bytes of output dropped from memory; see `session log`]\n",
                self.base - from
            ));
        }
        let (raw, cut) = self.without_tui(from, to);
        let mut body = text::normalize_terminal(&raw);
        // End markers of runs a nested shell took over (and their begin lines) are noise.
        if body.contains("__XSSH_") {
            body = STALE_MARKER.replace_all(&body, "").into_owned();
        }
        if cut && !self.tui_active {
            self.events
                .push("full-screen program output left out (its screens were returned while it ran)".into());
        }
        self.reader(reader).cursor = if done.is_some() && !owner { to } else { total };
        match done {
            Some(code) => {
                if owner {
                    self.run = None;
                }
                // The end marker is printed after a newline of its own.
                if body.ends_with('\n') {
                    body.pop();
                }
                out.push_str(&body);
                (out, Some(code), true)
            }
            None => {
                out.push_str(&body);
                (out, None, false)
            }
        }
    }

    /// Visible screen text (compacted, secrets masked). For a program started by `session run`,
    /// rows above its begin marker (shell history, the run wrapper) are left out.
    fn screen_text(&self) -> String {
        let contents = render_screen(self.parser.screen());
        let rows: Vec<&str> = contents.lines().collect();
        let from = self
            .run
            .as_ref()
            .and_then(|r| rows.iter().rposition(|l| l.contains(&r.begin_marker)))
            .map(|i| i + 1)
            .unwrap_or(0);
        let visible: Vec<&str> = rows[from..]
            .iter()
            .copied()
            .filter(|l| !l.contains("__XSSH_") && !l.contains(marker::HOOK_FN))
            .collect();
        text::redact(&text::compact_screen(&visible.join("\n")), &self.redact)
    }

    fn busy(&self, screen: &str) -> bool {
        self.busy_patterns.iter().any(|re| re.is_match(screen))
    }

    /// Track screen changes after new output, ignoring animation (spinners, counters).
    fn update_screen_sig(&mut self) {
        // Rendered with highlight markers, so moving a menu selection counts as a change.
        let sig = screen_signature(&render_screen(self.parser.screen()));
        if sig != self.screen_sig {
            self.screen_sig = sig;
            self.screen_changed_at = Instant::now();
        }
    }

    /// Number of lines currently held in the terminal's scrollback.
    fn scrollback_len(&mut self) -> usize {
        let s = self.parser.screen_mut();
        s.set_scrollback(usize::MAX);
        let n = s.scrollback();
        s.set_scrollback(0);
        n
    }

    /// The newest `n` scrollback lines, oldest first.
    fn scrollback_lines(&mut self, n: usize) -> Vec<String> {
        let (rows, cols) = self.parser.screen().size();
        let rows = rows as usize;
        let mut lines: Vec<String> = Vec::with_capacity(n);
        // With a scrollback offset of `off`, visible row r shows the line `off - r` above the live screen.
        let mut off = n;
        while off > 0 {
            self.parser.screen_mut().set_scrollback(off);
            let k = off.min(rows);
            lines.extend(self.parser.screen().rows(0, cols).take(k).map(|l| l.trim_end().to_string()));
            off -= k;
        }
        self.parser.screen_mut().set_scrollback(0);
        lines
    }

    /// Lines that scrolled off the top of the screen since the reader's last call (oldest first).
    fn take_scrolled(&mut self, reader: &str) -> String {
        const MAX_LINES: usize = 2000;
        const ANCHOR: usize = 3;
        let now = self.scrollback_len();
        let (seen, anchor) = {
            let r = self.reader(reader);
            (r.sb_seen, r.sb_anchor.clone())
        };
        let new = if now < SCROLLBACK || anchor.is_empty() {
            now.saturating_sub(seen)
        } else {
            // The scrollback is full, so its length no longer grows: find the newest lines
            // reported last time and count what came after them.
            let all = self.scrollback_lines(now);
            (anchor.len()..=all.len())
                .rev()
                .find(|&end| all[end - anchor.len()..end] == anchor[..])
                .map(|end| all.len() - end)
                .unwrap_or(all.len())
        };
        let take = new.min(MAX_LINES);
        let lines = if take > 0 { self.scrollback_lines(take) } else { vec![] };
        let newest = if take > 0 {
            lines[lines.len().saturating_sub(ANCHOR)..].to_vec()
        } else {
            anchor
        };
        {
            let r = self.reader(reader);
            r.sb_seen = now;
            r.sb_anchor = newest;
        }
        if new == 0 {
            return String::new();
        }
        let mut out = String::new();
        if new > take {
            out.push_str(&format!("[{} earlier lines omitted]\n", new - take));
        }
        out.push_str(&text::redact(&lines.join("\n"), &self.redact));
        out
    }
}

/// Render the screen as text, wrapping highlighted runs in «…»: text whose background or
/// reverse-video differs from the dominant style of the screen's text. That is how menus and
/// dialogs (dialog, whiptail, curses apps) show the selected item and button.
fn render_screen(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    let style = |c: &vt100::Cell| (format!("{:?}", c.bgcolor()), c.inverse());
    let mut counts: HashMap<(String, bool), usize> = HashMap::new();
    for r in 0..rows {
        for c in 0..cols {
            if let Some(cell) = screen.cell(r, c)
                && !cell.contents().trim().is_empty()
            {
                *counts.entry(style(cell)).or_default() += 1;
            }
        }
    }
    let dominant = counts.into_iter().max_by_key(|(_, n)| *n).map(|(s, _)| s);
    let mut out = String::with_capacity((rows as usize) * (cols as usize + 1));
    for r in 0..rows {
        let mut line = String::new();
        let mut open = false;
        let mut pending_space = String::new();
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let text = cell.contents();
            let text = if text.is_empty() { " " } else { text };
            let hl = dominant.as_ref().is_some_and(|d| &style(cell) != d);
            if text.trim().is_empty() {
                // Spaces inside a highlighted run stay inside; decided when the next text arrives.
                pending_space.push_str(text);
                continue;
            }
            if hl && !open {
                line.push_str(&pending_space);
                line.push('«');
                open = true;
            } else if !hl && open {
                line.push('»');
                line.push_str(&pending_space);
                open = false;
            } else {
                line.push_str(&pending_space);
            }
            pending_space.clear();
            line.push_str(text);
        }
        if open {
            line.push('»');
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Hash of the screen with animation normalized away: digits (timers, token counts) and
/// spinner glyphs (braille, dingbat stars, dots, circle quarters) do not count as changes.
fn screen_signature(contents: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in contents.chars() {
        let n = match c {
            '0'..='9' => '0',
            '\u{2800}'..='\u{28FF}' | '\u{2722}'..='\u{274B}' | '\u{25D0}'..='\u{25D3}' | '·' | '•' | '∙' | '⋅' => '*',
            c => c,
        };
        n.hash(&mut h);
    }
    h.finish()
}

/// Scrollback rows kept by the screen model (lines that scroll off an inline TUI).
const SCROLLBACK: usize = 3000;

/// How this output chunk redraws the screen in place.
fn redraw_kind(data: &[u8]) -> Redraw {
    let has = |pat: &[u8]| data.windows(pat.len()).any(|w| w == pat);
    if has(b"\x1b[H") || has(b"\x1b[2J") {
        return Redraw::Full;
    }
    let cursor_up = data
        .windows(4)
        .any(|w| w[0] == 0x1b && w[1] == b'[' && w[2].is_ascii_digit() && (w[3] == b'A' || w[3] == b'F'))
        || has(b"\x1b[A");
    if cursor_up && (has(b"\x1b[2K") || has(b"\x1b[K")) {
        Redraw::Inline
    } else {
        Redraw::None
    }
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Where input goes: the PTY, or a tmux pane through control-mode commands.
#[derive(Clone)]
enum Backend {
    Pty,
    Tmux { target: String, name: String },
}

/// Writes input for a backend (shared with the reader task for auto-responses).
#[derive(Clone)]
struct Input {
    writer: Arc<ChannelWriteHalf<Msg>>,
    backend: Backend,
}

impl Input {
    async fn write(&self, bytes: &[u8]) -> Result<()> {
        let data = match &self.backend {
            Backend::Pty => bytes.to_vec(),
            Backend::Tmux { target, .. } => tmux::send_keys(target, bytes),
        };
        self.writer
            .data_bytes(data)
            .await
            .map_err(|e| Error::remote(format!("write to session: {e}")))
    }
}

/// Settings shared by every session of a daemon.
pub struct OpenCtx<'a> {
    pub paths: &'a Paths,
    pub max_output: usize,
    pub default_size: (u16, u16),
    /// Who opens the session (recorded as its owner).
    pub owner: &'a str,
}

pub struct Session {
    pub id: String,
    pub name: Option<String>,
    pub host: String,
    pub created_at: String,
    pub transcript_path: String,
    /// The reader (agent) that opened it.
    pub owner: String,
    cols: Mutex<(u16, u16)>,
    input: Input,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    op_lock: tokio::sync::Mutex<()>,
    /// Who holds `op_lock`, doing what, since when (for busy answers).
    holder: Mutex<Option<(String, String, Instant)>>,
    /// Last command or input sent, so agents can tell sessions apart (`session list`).
    last: Mutex<String>,
    max_output: usize,
    paths: Paths,
    persist: Option<tmux::PersistRec>,
    conn: Arc<Conn>,
}

static SETTLE: Duration = Duration::from_millis(300);

/// Login shells xssh cannot drive (no POSIX syntax): sessions run `bash -l` (or `sh -l`) instead.
const NON_POSIX_SHELLS: &[&str] = &["fish", "csh", "tcsh", "nu", "elvish", "xonsh", "pwsh", "powershell", "rc", "es"];

/// What `sh` finds out about the account before a session starts.
struct Probe {
    shell: String,
    tmux: bool,
    bash: bool,
}

impl Probe {
    /// The POSIX shell to run instead of a non-POSIX login shell.
    fn replacement(&self) -> Option<&'static str> {
        let base = self.shell.rsplit('/').next().unwrap_or("");
        NON_POSIX_SHELLS
            .contains(&base)
            .then_some(if self.bash { "bash -l" } else { "sh -l" })
    }
}

/// Run a command on its own channel and collect stdout (no PTY). The login shell parses
/// `command`: callers that need POSIX syntax wrap it with [`sh_c`].
async fn exec_capture(conn: &Conn, command: &str, timeout: Duration) -> Result<(String, Option<u32>)> {
    let fut = async {
        let mut ch = conn.open_session().await?;
        ch.exec(true, command.as_bytes().to_vec()).await?;
        let mut out = Vec::new();
        let mut code = None;
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                _ => {}
            }
        }
        Ok::<_, Error>((String::from_utf8_lossy(&out).into_owned(), code))
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| Error::new(ErrorCode::Timeout, format!("remote command timed out: {command}")))?
}

fn sh_c(script: &str) -> String {
    format!("sh -c {}", shq(script))
}

async fn probe(conn: &Conn) -> Result<Probe> {
    let (out, _) = exec_capture(
        conn,
        &sh_c(r#"printf 'XSSH_SHELL=%s\nXSSH_TMUX=%s\nXSSH_BASH=%s\n' "$SHELL" "$(command -v tmux)" "$(command -v bash)""#),
        Duration::from_secs(15),
    )
    .await?;
    let get = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(k))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    Ok(Probe {
        shell: get("XSSH_SHELL="),
        tmux: !get("XSSH_TMUX=").is_empty(),
        bash: !get("XSSH_BASH=").is_empty(),
    })
}

/// Everything [`Session::start`] needs besides the channel.
struct Start {
    id: String,
    name: Option<String>,
    host: String,
    created_at: String,
    owner: String,
    cols: u16,
    rows: u16,
    encoding: Option<String>,
    no_autofill: bool,
    busy_patterns: Vec<String>,
    transcript_path: std::path::PathBuf,
    persist: Option<tmux::PersistRec>,
    /// Terminal bytes that recreate the current screen (tmux attach).
    seed: Option<Vec<u8>>,
    /// Reattach: whether the shell is at its prompt (None: a new shell).
    at_prompt: Option<bool>,
}

impl Session {
    /// Open a new session (PTY, or a new tmux session with `p.persist`).
    pub async fn open(conn: Arc<Conn>, p: &OpenParams, secrets: SessionSecrets, ctx: &OpenCtx<'_>) -> Result<Arc<Session>> {
        let cols = p.cols.unwrap_or(ctx.default_size.0).clamp(20, 1000);
        let rows = p.rows.unwrap_or(ctx.default_size.1).clamp(5, 500);
        let id = format!("s{}", short_id(5));
        let pr = probe(&conn).await?;
        let replacement = pr.replacement();
        let mut events = vec![];
        if let Some(r) = replacement {
            events.push(format!(
                "login shell is {} (not POSIX); this session runs `{r}` instead",
                pr.shell.rsplit('/').next().unwrap_or(&pr.shell)
            ));
        }
        let key = p.name.clone().unwrap_or_else(|| id.clone());
        let transcript_path = ctx.paths.sessions_dir().join(transcript_file(&id, p.name.as_deref()));
        let created_at = xssh_store::audit::now_rfc3339();
        let (persist, channel, seed) = if p.persist {
            if !pr.tmux {
                return Err(Error::new(ErrorCode::Usage, format!("--persist needs tmux on '{}', which is not installed", p.host)).hint(
                    "install tmux on the host (e.g. `xssh exec HOST --sudo -- apt-get install -y tmux`), or open the session without --persist",
                ));
            }
            let name = tmux::session_name(&key);
            let (out, code) = exec_capture(
                &conn,
                &sh_c(&format!("{} 2>&1", tmux::create_cmd(&name, cols, rows, replacement))),
                Duration::from_secs(15),
            )
            .await?;
            if code != Some(0) {
                let msg = out.trim();
                if msg.contains("duplicate session") {
                    return Err(Error::new(
                        ErrorCode::AlreadyExists,
                        format!("tmux session {name} already exists on '{}'", p.host),
                    )
                    .hint(format!(
                        "pick another --name, or end the old one: `xssh exec {} -- tmux kill-session -t {name}`",
                        p.host
                    )));
                }
                return Err(Error::remote(format!("could not start tmux session {name}: {msg}")));
            }
            let rec = tmux::PersistRec {
                id: id.clone(),
                name: p.name.clone(),
                host: p.host.clone(),
                tmux: name,
                owner: ctx.owner.to_string(),
                created_at: created_at.clone(),
                transcript: transcript_path.display().to_string(),
                cols,
                rows,
                encoding: p.encoding.clone(),
                no_autofill: p.no_autofill,
            };
            let (ch, seed, _) = match attach_tmux(&conn, &rec).await {
                Ok(x) => x,
                Err(e) => {
                    // Do not leave the new tmux session behind.
                    let _ = exec_capture(&conn, &sh_c(&tmux::kill_cmd(&rec.tmux)), Duration::from_secs(10)).await;
                    return Err(e);
                }
            };
            (Some(rec), ch, seed)
        } else {
            let ch = conn.open_session().await?;
            ch.request_pty(
                true,
                p.term.as_deref().unwrap_or("xterm-256color"),
                cols as u32,
                rows as u32,
                0,
                0,
                &[],
            )
            .await?;
            match replacement {
                Some(r) => ch.exec(true, format!("exec {r}").into_bytes()).await?,
                None => ch.request_shell(true).await?,
            }
            (None, ch, None)
        };
        let s = Self::start(
            conn,
            channel,
            Start {
                id,
                name: p.name.clone(),
                host: p.host.clone(),
                created_at,
                owner: ctx.owner.to_string(),
                cols,
                rows,
                encoding: p.encoding.clone(),
                no_autofill: p.no_autofill,
                busy_patterns: p.busy_patterns.clone(),
                transcript_path,
                persist,
                seed,
                at_prompt: None,
            },
            secrets,
            ctx,
        )
        .await?;
        // The prompt hook, typed into the fresh shell (typeahead is fine).
        s.inject(&marker::setup_line()).await?;
        s.state.lock().unwrap().events.extend(events);
        Ok(s)
    }

    /// Attach again to a persistent session's tmux session (after a daemon restart or a
    /// dropped connection).
    pub async fn reattach(
        conn: Arc<Conn>,
        rec: &tmux::PersistRec,
        secrets: SessionSecrets,
        ctx: &OpenCtx<'_>,
        busy_patterns: Vec<String>,
    ) -> Result<Arc<Session>> {
        let (ch, seed, at_prompt) = match attach_tmux(&conn, rec).await {
            Ok(x) => x,
            Err(e) => {
                if e.code == ErrorCode::NotFound {
                    let _ = tmux::remove(ctx.paths, &rec.id);
                }
                return Err(e);
            }
        };
        let s = Self::start(
            conn,
            ch,
            Start {
                id: rec.id.clone(),
                name: rec.name.clone(),
                host: rec.host.clone(),
                created_at: rec.created_at.clone(),
                owner: rec.owner.clone(),
                cols: rec.cols,
                rows: rec.rows,
                encoding: rec.encoding.clone(),
                no_autofill: rec.no_autofill,
                busy_patterns,
                transcript_path: rec.transcript.clone().into(),
                persist: Some(rec.clone()),
                seed,
                at_prompt: Some(at_prompt),
            },
            secrets,
            ctx,
        )
        .await?;
        {
            let mut st = s.state.lock().unwrap();
            st.note("reattached");
            st.events.push(format!(
                "reattached to persistent session (tmux {}): the shell kept running while detached; the screen shows its current state",
                rec.tmux
            ));
        }
        Ok(s)
    }

    async fn start(
        conn: Arc<Conn>,
        channel: russh::Channel<Msg>,
        o: Start,
        secrets: SessionSecrets,
        ctx: &OpenCtx<'_>,
    ) -> Result<Arc<Session>> {
        let (mut reader, writer) = channel.split();
        let writer = Arc::new(writer);
        let backend = match &o.persist {
            Some(r) => Backend::Tmux {
                target: tmux::pane_target(&r.tmux),
                name: r.tmux.clone(),
            },
            None => Backend::Pty,
        };
        let input = Input { writer, backend };
        if matches!(input.backend, Backend::Tmux { .. }) {
            input
                .writer
                .data_bytes(tmux::resize(o.cols, o.rows))
                .await
                .map_err(|e| Error::remote(e.to_string()))?;
        }
        let transcript = std::fs::OpenOptions::new().create(true).append(true).open(&o.transcript_path).ok();

        let mut responders = vec![];
        for (rule, reply) in secrets.responders {
            let re = Regex::new(&rule.pattern)?;
            responders.push(Responder {
                re,
                reply,
                enter: rule.enter,
                label: rule.pattern.clone(),
            });
        }
        let mut st = State::new(o.rows, o.cols, o.encoding.as_deref());
        st.max_secret = secrets.redact.iter().map(String::len).max().unwrap_or(0);
        st.redact = secrets.redact;
        st.responders = responders;
        st.sudo_password = if o.no_autofill { None } else { secrets.sudo_password };
        st.busy_patterns = o
            .busy_patterns
            .iter()
            .map(|b| Regex::new(b))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        st.transcript = transcript;
        if let Some(seed) = &o.seed {
            // The current tmux screen, drawn into the model only (not the text transcript).
            st.parser.process(seed);
            st.update_screen_sig();
        }
        if let Some(at_prompt) = o.at_prompt {
            // A reattached shell already has its hook.
            st.hooked = true;
            st.submitted = !at_prompt;
        }
        let state = Arc::new(Mutex::new(st));
        let notify = Arc::new(Notify::new());

        // Reader task: feeds the screen model and transcript, runs auto-responders.
        {
            let state = state.clone();
            let notify = notify.clone();
            let input = input.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                let tmux = matches!(input.backend, Backend::Tmux { .. });
                let mut line_buf: Vec<u8> = Vec::new();
                loop {
                    let msg = reader.wait().await;
                    let mut replies: Vec<Vec<u8>> = vec![];
                    let ended;
                    {
                        let mut st = state.lock().unwrap();
                        match msg {
                            Some(ChannelMsg::Data { data }) if tmux => {
                                line_buf.extend_from_slice(&data);
                                while let Some(nl) = line_buf.iter().position(|&b| b == b'\n') {
                                    let line: Vec<u8> = line_buf.drain(..=nl).collect();
                                    match tmux::parse_line(&line[..line.len() - 1]) {
                                        tmux::Event::Output(bytes) => replies.extend(st.ingest_bytes(&bytes)),
                                        tmux::Event::Exit => st.tmux_exit = true,
                                        tmux::Event::Other => {}
                                    }
                                }
                            }
                            // tmux's own stderr is not pane output; keep it in the transcript.
                            Some(ChannelMsg::ExtendedData { data, .. }) if tmux => {
                                st.note(&format!("tmux: {}", String::from_utf8_lossy(&data).trim()))
                            }
                            Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                                replies.extend(st.ingest_bytes(&data));
                            }
                            Some(ChannelMsg::ExitStatus { exit_status }) => st.exit_code = Some(exit_status),
                            // sshd sends EOF before exit-status: keep reading until the channel closes.
                            Some(ChannelMsg::Eof) => st.flush_transcript(true),
                            Some(ChannelMsg::Close) | None => {
                                if tmux {
                                    if st.tmux_exit && !st.detached {
                                        // The tmux session ended (its shell exited, or it was killed).
                                        st.closed = true;
                                    } else if !st.closed {
                                        // The control client went away; the tmux session did not.
                                        st.detached = true;
                                    }
                                } else if !st.closed {
                                    st.closed = true;
                                    st.disconnected = st.exit_code.is_none() && conn.is_closed();
                                }
                                st.flush_transcript(true);
                            }
                            Some(_) => {}
                        }
                        ended = st.closed || st.detached;
                    }
                    for bytes in replies {
                        let _ = input.write(&bytes).await;
                    }
                    notify.notify_waiters();
                    if ended {
                        break;
                    }
                }
            });
        }

        Ok(Arc::new(Session {
            id: o.id,
            name: o.name,
            host: o.host,
            created_at: o.created_at,
            transcript_path: o.transcript_path.display().to_string(),
            owner: o.owner,
            cols: Mutex::new((o.cols, o.rows)),
            input,
            state,
            notify,
            op_lock: tokio::sync::Mutex::new(()),
            holder: Mutex::new(None),
            last: Mutex::new(String::new()),
            max_output: ctx.max_output,
            paths: ctx.paths.clone(),
            persist: o.persist,
            conn,
        }))
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// The connection is gone (the session cannot be used; a persistent one can be reattached).
    pub fn is_disconnected(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.detached || st.disconnected || (!st.closed && self.conn.is_closed())
    }

    pub fn persist_rec(&self) -> Option<&tmux::PersistRec> {
        self.persist.as_ref()
    }

    /// Time since agents last used the session or it last printed anything.
    pub fn idle_for(&self) -> Duration {
        let st = self.state.lock().unwrap();
        st.last_activity.elapsed().min(st.last_output.elapsed())
    }

    /// A `session run` command has not finished.
    pub fn run_pending(&self) -> bool {
        let st = self.state.lock().unwrap();
        !st.closed && st.run.as_ref().is_some_and(|r| r.end.is_none())
    }

    fn touch(&self) {
        self.state.lock().unwrap().last_activity = Instant::now();
    }

    pub fn info(&self) -> SessionInfo {
        let st = self.state.lock().unwrap();
        // A listing reports what is true now: at the marked prompt means idle, however recent.
        let idle = if st.at_marker() { Duration::ZERO } else { SETTLE };
        let (mut state, prompt, _) = st.state_now(idle);
        if state != SessionState::Exited && !st.closed && self.conn.is_closed() {
            state = SessionState::Disconnected;
        }
        let (cols, rows) = *self.cols.lock().unwrap();
        SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            created_at: self.created_at.clone(),
            idle_secs: st.last_activity.elapsed().as_secs(),
            state,
            cols,
            rows,
            transcript: self.transcript_path.clone(),
            pending_run: st.run.as_ref().filter(|r| !st.closed && r.end.is_none()).map(|r| r.nonce.clone()),
            last: self.last.lock().unwrap().clone(),
            cmd: st.last_run.clone(),
            prompt: prompt.filter(|_| state == SessionState::WaitingInput && !st.tui()),
            owner: self.owner.clone(),
            persistent: self.persist.is_some(),
        }
    }

    /// How agents refer to this session: its name when it has one, else its id.
    pub fn key(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// Text for the audit log and transcript for input sent with `session send`: withheld when
    /// the session is asking for a password, pastes summarized, known secrets masked.
    pub fn input_summary(&self, text: Option<&str>, paste: bool, keys: Option<&str>, enter: bool) -> String {
        let st = self.state.lock().unwrap();
        let (_, kind, _) = st.state_now(Duration::ZERO);
        let mut s = match text {
            Some(t) if kind == Some(PromptKind::Password) && !st.tui() => {
                format!("[{} chars withheld: password prompt]", t.chars().count())
            }
            Some(t) if paste => format!("[paste: {} chars, {} lines]", t.chars().count(), t.lines().count().max(1)),
            Some(t) => text::redact(t, &st.redact),
            None => String::new(),
        };
        if let Some(k) = keys {
            s.push_str(&format!(" [keys:{k}]"));
        }
        if enter {
            s.push_str(" [enter]");
        }
        s.trim().to_string()
    }

    /// Mask known secrets (for audit records of commands).
    pub fn redact(&self, s: &str) -> String {
        text::redact(s, &self.state.lock().unwrap().redact)
    }

    /// Record keys/text sent with `session send` in the transcript.
    pub fn note_input(&self, s: &str) {
        let full: String = s.chars().take(300).collect();
        self.state.lock().unwrap().note(&format!("input {full}"));
    }

    /// Remember what was last sent (one line, clipped).
    pub fn set_last(&self, s: &str) {
        let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let clipped: String = line.chars().take(60).collect();
        *self.last.lock().unwrap() = if clipped.len() < line.len() {
            format!("{clipped}...")
        } else {
            clipped
        };
    }

    /// Take the operation lock, waiting at most `timeout`: a caller queued behind another
    /// agent's long `run` gets a prompt, explicit busy answer instead of blocking past its own
    /// tool timeout.
    async fn lock_op(&self, reader: &str, what: &str, timeout: Duration) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        let guard = match self.op_lock.try_lock() {
            Ok(g) => g,
            Err(_) => match tokio::time::timeout(timeout, self.op_lock.lock()).await {
                Ok(g) => g,
                Err(_) => {
                    let (who, doing, since) = self
                        .holder
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| ("another agent".into(), "a command".into(), Instant::now()));
                    let who = if who == reader {
                        "an earlier call of yours".to_string()
                    } else {
                        format!("agent '{who}'")
                    };
                    return Err(Error::new(
                        ErrorCode::Busy,
                        format!(
                            "session {} is busy: {who} is running `{doing}` (for {}s)",
                            self.key(),
                            since.elapsed().as_secs()
                        ),
                    )
                    .hint(format!(
                        "`xssh session read {0}` shows its output without waiting; retry later, or open your own session",
                        self.key()
                    )));
                }
            },
        };
        let what: String = what.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
        *self.holder.lock().unwrap() = Some((reader.to_string(), what, Instant::now()));
        Ok(guard)
    }

    fn ensure_usable(&self) -> Result<()> {
        let st = self.state.lock().unwrap();
        if st.detached || st.disconnected || (!st.closed && self.conn.is_closed()) {
            return Err(disconnected_err(self.key(), self.persist.is_some()));
        }
        if st.closed {
            return Err(closed_err(&self.id));
        }
        Ok(())
    }

    /// Mark a line submitted when the input contains Enter.
    fn note_submit(&self, bytes: &[u8]) {
        if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
            self.state.lock().unwrap().submitted = true;
        }
    }

    /// Wait until output has been quiet for `idle` (after at least some output
    /// arrived past `mark`, when `need_new`), the session closed, or `timeout`.
    async fn settle(&self, mark: u64, need_new: bool, idle: Duration, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let st = self.state.lock().unwrap();
                if st.closed || st.detached {
                    return;
                }
                let has_new = st.total() > mark;
                let quiet = st.last_output.elapsed();
                if (has_new || !need_new) && quiet >= idle {
                    return;
                }
                // Prompts that ask for input settle faster; so does the shell prompt (marker).
                if has_new && quiet >= Duration::from_millis(150) && idle > Duration::from_millis(150) {
                    let line = st.current_line();
                    if st.at_marker() || matches!(prompt::classify(&line), Some(PromptKind::Password | PromptKind::Confirm)) {
                        return;
                    }
                }
                if has_new || !need_new {
                    idle.saturating_sub(quiet).max(Duration::from_millis(20))
                } else {
                    idle
                }
            };
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return;
            }
            let wait = wait.min(deadline - now);
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Wait (up to `max`) for the shell's prompt marker after a submitted line, so a command
    /// that just finished is not mistaken for one still running.
    async fn wait_prompt_marker(&self, max: Duration) {
        let deadline = tokio::time::Instant::now() + max;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let st = self.state.lock().unwrap();
                let pending = st.run.as_ref().is_some_and(|r| r.end.is_none());
                if st.closed || st.detached || !st.hooked || st.at_marker() || pending || st.tui() {
                    return;
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep((deadline - now).min(Duration::from_millis(100))) => {}
            }
        }
    }

    /// Consume the reader's output and build an observation.
    fn observe(&self, st: &mut State, reader: &str, idle: Duration) -> Observation {
        let (out, exit_code, completed) = st.take_output(reader);
        self.build(st, reader, out, idle, exit_code, completed)
    }

    fn build(&self, st: &mut State, reader: &str, out: String, idle: Duration, exit_code: Option<i64>, completed: bool) -> Observation {
        let (mut state, mut kind, line) = st.state_now(idle);
        let tui = st.tui() && !matches!(state, SessionState::Exited | SessionState::Disconnected);
        let events = st.take_events(reader);
        // A TUI's byte stream is a series of redraws: report the lines that scrolled off the
        // top (finished content) plus the live screen, and skip the screen when unchanged.
        let (out, screen, screen_unchanged, busy) = if tui && !completed {
            let scrolled = st.take_scrolled(reader);
            let screen = st.screen_text();
            let busy = st.busy(&screen);
            let r = st.reader(reader);
            let unchanged = r.last_screen_shown.as_deref() == Some(screen.as_str());
            r.last_screen_shown = Some(screen.clone());
            if !unchanged {
                st.note(&format!("screen\n{screen}\n{NOTE}screen-end"));
            }
            kind = None;
            if busy || st.screen_changed_at.elapsed() < Duration::from_millis(400) {
                state = SessionState::Running;
            }
            (scrolled, (!unchanged).then_some(screen), unchanged, busy)
        } else {
            st.reader(reader).last_screen_shown = None;
            let mut out = trim_lines(&out);
            // Blank lines before the prompt (zsh's PROMPT_SP, `echo` at the end) carry nothing.
            while out.ends_with("\n\n") {
                out.pop();
            }
            (out, None, false, false)
        };
        let t = text::truncate(out, self.max_output, Some(&self.paths), &format!("{}-{}", self.host, self.id));
        let (key, hint): (&'static str, Option<String>) = if tui {
            (
                "tui",
                Some(format!(
                    "full-screen program: `output` = lines that scrolled off, then the live screen. Type with `session send {0} 'text' --enter`, \
                     keys with `--keys`; `--until REGEX` / `--stable 2s` wait for the result; quit the program before the next `session run`",
                    self.key()
                )),
            )
        } else {
            match (state, kind) {
                (SessionState::Disconnected, _) => ("disconnected", disconnected_err(self.key(), self.persist.is_some()).hint),
                (SessionState::WaitingInput, Some(PromptKind::Password)) => (
                    "password",
                    Some(
                        "password requested and nothing auto-filled it (sudo prompts are filled when a password is stored). \
                         Ask the user, or store it (`xssh secret set NAME`) and reopen with `--respond 'REGEX=>{secret:NAME}'`"
                            .into(),
                    ),
                ),
                (SessionState::WaitingInput, Some(PromptKind::Pager)) => {
                    ("pager", Some("pager active: `--keys q` quits, `--keys space` pages".into()))
                }
                (SessionState::Quiet, None) if st.run.is_some() => (
                    "quiet",
                    Some(format!(
                        "no new output for {}s and no recognizable prompt: still working, or waiting at an unusual prompt. `session read` polls, `--keys ctrl-c` interrupts",
                        st.last_output.elapsed().as_secs()
                    )),
                ),
                _ => ("", None),
            }
        };
        let hint = hint.filter(|_| st.reader(reader).hinted.insert(key));
        let current_line = text::redact(line.trim_end(), &st.redact);
        Observation {
            session: self.key().to_string(),
            output: t.text,
            state,
            prompt: kind,
            current_line,
            exit_code,
            completed,
            events,
            alt_screen: tui,
            screen,
            screen_unchanged,
            busy,
            timed_out: false,
            unfinished: !completed && !st.closed && st.run.as_ref().is_some_and(|r| r.end.is_none()),
            hint,
            truncated_output_path: t.full_output_path,
        }
    }

    /// Wait for a screen condition (see [`WaitSpec`]); a TUI with no explicit condition waits
    /// for the screen to be stable. Returns false on timeout.
    async fn settle_screen(&self, mark: u64, w: &WaitSpec, timeout: Duration, after_input: bool) -> Result<bool> {
        let until = w.until.as_deref().map(Regex::new).transpose()?;
        let gone = w.gone.as_deref().map(Regex::new).transpose()?;
        let stable = Duration::from_millis(w.stable_ms.unwrap_or(1500));
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        // Text already on screen before the input does not satisfy `--until`: only new output,
        // or the screen after it changed.
        let before = match &until {
            Some(re) if after_input => re.is_match(&self.state.lock().unwrap().screen_text()),
            _ => false,
        };
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let st = self.state.lock().unwrap();
                if st.closed || st.detached {
                    return Ok(true);
                }
                let screen = st.screen_text();
                let met = if let Some(re) = &until {
                    // Also look at the stream, in case the match already scrolled off.
                    (re.is_match(&screen) && (!before || st.screen_changed_at > started))
                        || re.is_match(&text::normalize_terminal(&st.text[st.idx(mark)..]))
                } else if let Some(re) = &gone {
                    !re.is_match(&screen) && st.screen_changed_at.elapsed() >= Duration::from_millis(300)
                } else {
                    let quiet = st.screen_changed_at.elapsed().min(started.elapsed());
                    quiet >= stable && !st.busy(&screen)
                };
                if met {
                    return Ok(true);
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep((deadline - now).min(Duration::from_millis(100))) => {}
            }
        }
    }

    fn timed_out_hint(&self, obs: &mut Observation, w: &WaitSpec, timeout: Duration) {
        obs.timed_out = true;
        let what = match (&w.until, &w.gone) {
            (Some(u), _) => format!("{u:?} did not appear"),
            (_, Some(g)) => format!("{g:?} did not disappear"),
            _ if obs.busy => "the program is still busy".into(),
            _ => "the screen did not settle".into(),
        };
        obs.hint = Some(format!(
            "{what} within {}s (exit 124). `session wait {}` keeps waiting",
            timeout.as_secs(),
            self.key()
        ));
    }

    /// Return output produced since this reader's last call. Reading never waits for another
    /// agent's command to finish (no operation lock).
    pub async fn read(&self, reader: &str, idle: Duration, timeout: Duration, need_new: bool, wait: &WaitSpec) -> Result<Observation> {
        self.touch();
        let tui = self.state.lock().unwrap().tui();
        // A full-screen program: `session wait` waits for a stable, not busy screen (below).
        if wait.run_end && !wait.is_screen_wait() && !tui {
            // `session wait`: a `run` still in flight -> wait for its end marker.
            if self.run_pending() {
                return self.await_run(reader, timeout, Instant::now()).await;
            }
            // A command typed with `send ... --enter` -> wait for the shell prompt marker.
            let typed = {
                let st = self.state.lock().unwrap();
                st.hooked && !st.at_marker() && !st.tui() && !st.closed && !st.detached
            };
            if typed {
                return self.await_prompt(reader, timeout).await;
            }
        }
        let (mark, screen_mode) = {
            let mut st = self.state.lock().unwrap();
            let c = st.reader(reader).cursor;
            (c, wait.is_screen_wait() || st.tui())
        };
        let met = if screen_mode {
            self.settle_screen(mark, wait, timeout, false).await?
        } else {
            // Nothing running for `session wait`: report the current state right away.
            self.settle(mark, need_new && !wait.run_end, idle, timeout).await;
            true
        };
        let mut st = self.state.lock().unwrap();
        let mut obs = self.observe(&mut st, reader, idle);
        if !met {
            self.timed_out_hint(&mut obs, wait, timeout);
        }
        self.closed_exit(&st, &mut obs);
        Ok(obs)
    }

    /// Wait for the shell prompt after a command typed with `send`: its exit status comes with
    /// the prompt marker. Returns early when the command asks for input.
    async fn await_prompt(&self, reader: &str, timeout: Duration) -> Result<Observation> {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.state.lock().unwrap();
                let back = st.at_marker();
                let (s, k, _) = st.state_now(Duration::from_millis(400));
                // A question, or a nested shell's prompt (no marker): the agent must act.
                let asking = started.elapsed() >= Duration::from_millis(300) && st.asks_input(s, k);
                let timed_out = tokio::time::Instant::now() >= deadline;
                if back || asking || timed_out || st.closed || st.detached || st.tui() {
                    let code = st.last_prompt_exit.filter(|_| back);
                    let mut obs = self.observe(&mut st, reader, Duration::from_millis(150));
                    if back {
                        obs.exit_code = code;
                        obs.completed = code.is_some();
                    } else if timed_out {
                        obs.timed_out = true;
                        obs.hint = Some(format!(
                            "the command did not finish within {}s (exit 124). `session wait {}` keeps waiting",
                            timeout.as_secs(),
                            self.key()
                        ));
                    }
                    self.closed_exit(&st, &mut obs);
                    return Ok(obs);
                }
            }
            let now = tokio::time::Instant::now();
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(deadline.saturating_duration_since(now).min(Duration::from_millis(250))) => {}
            }
        }
    }

    /// Type into the session. `chunks` are written with a short pause in between, so e.g. the
    /// Enter after a text is seen as a key press rather than part of a paste (Ink TUIs treat
    /// fast "text\r" as pasted text). With `if_prompt`, refuse to send unless the current prompt
    /// kind matches (guards against typing into an unexpected dialog).
    pub async fn send(
        &self,
        reader: &str,
        chunks: &[Vec<u8>],
        idle: Duration,
        timeout: Duration,
        if_prompt: Option<&str>,
        wait: &WaitSpec,
    ) -> Result<Observation> {
        let bytes: Vec<u8> = chunks.concat();
        let bytes = bytes.as_slice();
        // An interrupt must get through while another call holds the session (e.g. a long run).
        let interrupt_only = !bytes.is_empty() && bytes.iter().all(|&b| b == 0x03);
        let t0 = Instant::now();
        let _g = if interrupt_only {
            None
        } else {
            Some(self.lock_op(reader, &String::from_utf8_lossy(bytes), timeout).await?)
        };
        let timeout = timeout.saturating_sub(t0.elapsed()).max(Duration::from_secs(1));
        self.touch();
        self.ensure_usable()?;
        let mark = {
            let mut st = self.state.lock().unwrap();
            if let Some(want) = if_prompt {
                let (state, kind, line) = st.state_now(SETTLE);
                let tui = st.tui();
                let kind_name = kind
                    .and_then(|k| serde_json::to_value(k).ok())
                    .and_then(|v| v.as_str().map(String::from));
                let ok = match want {
                    "any" => state == SessionState::WaitingInput,
                    "screen" => tui,
                    w => !tui && kind_name.as_deref() == Some(w),
                };
                if !ok {
                    return Err(Error::new(
                        ErrorCode::Busy,
                        format!(
                            "not sent: expected prompt '{want}' but state={} prompt={} tui={tui} line={:?}",
                            serde_json::to_value(state)
                                .ok()
                                .and_then(|v| v.as_str().map(String::from))
                                .unwrap_or_default(),
                            kind_name.unwrap_or_else(|| "none".into()),
                            line.trim_end()
                        ),
                    )
                    .hint(format!(
                        "inspect with `xssh session screen {}`, then decide what to send",
                        self.key()
                    )));
                }
            }
            let s = String::from_utf8_lossy(bytes);
            if s.contains("sudo") {
                st.after_sudo = true;
            } else if s.contains('\r') || s.contains('\n') {
                st.after_sudo = false;
            }
            if bytes.contains(&0x03) && st.run.as_ref().is_some_and(|r| r.end.is_none()) {
                st.interrupted = true;
            }
            st.last_input_at = st.total();
            st.reader(reader);
            st.total()
        };
        for (i, c) in chunks.iter().filter(|c| !c.is_empty()).enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            self.note_submit(c);
            self.input.write(c).await?;
        }
        let screen_mode = wait.is_screen_wait() || self.state.lock().unwrap().tui();
        let met = if screen_mode {
            self.settle_screen(mark, wait, timeout, true).await?
        } else {
            self.settle(mark, true, idle, timeout).await;
            true
        };
        let mut st = self.state.lock().unwrap();
        let mut obs = self.observe(&mut st, reader, idle);
        if !met {
            self.timed_out_hint(&mut obs, wait, timeout);
        }
        // Drop the terminal's echo of what was just typed.
        let typed = String::from_utf8_lossy(bytes);
        let typed = typed.trim_end_matches(['\r', '\n']);
        if !typed.is_empty()
            && !typed.contains(['\r', '\n', '\x1b'])
            && let Some(rest) = obs.output.strip_prefix(typed)
            && (rest.is_empty() || rest.starts_with('\n'))
        {
            obs.output = rest.strip_prefix('\n').unwrap_or(rest).to_string();
        }
        // No prompt hook: an interrupted `run` is over once a shell prompt is back.
        if !st.hooked
            && bytes.contains(&0x03)
            && !obs.completed
            && st.run.as_ref().is_some_and(|r| r.end.is_none())
            && obs.state == SessionState::WaitingInput
            && obs.prompt == Some(PromptKind::Shell)
        {
            st.run = None;
            st.interrupted = false;
            obs.completed = true;
            obs.exit_code = Some(130);
            obs.events.push("command interrupted".into());
        }
        // A command typed at the shell prompt that already finished: report its status.
        if !obs.completed
            && obs.exit_code.is_none()
            && st.at_marker()
            && st.run.is_none()
            && (bytes.contains(&b'\r') || bytes.contains(&b'\n'))
            && !st.tui()
        {
            obs.exit_code = st.last_prompt_exit;
            obs.completed = obs.exit_code.is_some();
        }
        self.closed_exit(&st, &mut obs);
        Ok(obs)
    }

    /// Report the shell's exit status once the session has ended.
    fn closed_exit(&self, st: &State, obs: &mut Observation) {
        if st.closed && obs.exit_code.is_none() {
            obs.exit_code = st.exit_code.map(i64::from);
        }
    }

    /// Run a shell command with begin/end markers: returns exactly its output
    /// and exit code, while keeping shell state (cwd, env, venv) across calls.
    pub async fn run(&self, reader: &str, command: &str, timeout: Duration) -> Result<Observation> {
        let t0 = Instant::now();
        let _g = self.lock_op(reader, command, timeout).await?;
        self.touch();
        self.ensure_usable()?;
        // A command that just finished prints its prompt marker a moment later.
        self.wait_prompt_marker(Duration::from_millis(1500)).await;
        // A prompt-shaped line without a marker (a nested shell just started) is trusted once
        // the output has been quiet for a second: give it that second.
        let (maybe_nested, mark) = {
            let st = self.state.lock().unwrap();
            let nested = st.hooked && !st.at_marker() && !st.tui() && prompt::classify(&st.current_line()) == Some(PromptKind::Shell);
            (nested, st.total())
        };
        if maybe_nested {
            self.settle(mark, false, Duration::from_millis(1050), Duration::from_millis(2000))
                .await;
        }
        let timeout = timeout.saturating_sub(t0.elapsed()).max(Duration::from_secs(1));
        let nonce = short_id(8);
        let hook;
        {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return Err(closed_err(&self.id));
            }
            let unfinished = st.run.as_ref().is_some_and(|r| r.end.is_none());
            if let Some((_, code)) = st.run.as_ref().and_then(|r| r.end) {
                st.events
                    .push(format!("previous command finished (exit {code}); its unread output was skipped"));
            }
            // `run` types a shell command line: refuse when something else owns the terminal.
            let (s, k, _) = st.state_now(SETTLE);
            let at_marker = st.at_marker() && !st.tui();
            let regex_shell = s == SessionState::WaitingInput && k == Some(PromptKind::Shell) && !st.tui();
            if !(at_marker || regex_shell)
                && let Some((msg, hint)) = self.refusal(&st, s, k, unfinished)
            {
                return Err(Error::new(ErrorCode::Busy, msg).hint(hint));
            }
            if unfinished && let Some(old) = st.run.take() {
                // A nested shell (sudo -i, su, ssh, docker exec) is at its prompt: run inside it.
                // The outer command's end marker appears when that shell exits.
                if st.stale_runs.len() >= 16 {
                    st.stale_runs.remove(0);
                }
                st.stale_runs.push(old.nonce);
            }
            // Not at a marked prompt: install the hook in whatever shell is there.
            hook = !at_marker;
            st.after_sudo = command.contains("sudo");
            let total = st.total();
            st.reader(reader).cursor = total;
            st.last_input_at = total;
            st.submitted = true;
            st.interrupted = false;
            let line = command.split_whitespace().collect::<Vec<_>>().join(" ");
            st.last_run = line.chars().take(80).collect();
            let one_line = command.trim_end().replace('\r', "").replace('\n', "\\n");
            st.note(&format!("run {}", one_line.chars().take(2000).collect::<String>()));
            st.run = Some(RunTrack {
                nonce: nonce.clone(),
                begin_marker: format!("__XSSH_B_{nonce}"),
                end_re: Regex::new(&format!(r"__XSSH_E_{nonce}_(\d+)\r?\n")).unwrap(),
                begin: None,
                end: None,
                owner: reader.to_string(),
            });
        }
        self.set_last(command);
        let line = wrapper(command, &nonce, hook);
        self.input.write(&self.encode_text(&line)).await?;
        self.await_run(reader, timeout, Instant::now()).await
    }

    /// Why `run` must not type a command line now (None: go ahead).
    fn refusal(&self, st: &State, s: SessionState, k: Option<PromptKind>, unfinished: bool) -> Option<(String, String)> {
        let id = self.key();
        if st.tui() {
            return Some((
                "a full-screen program is running, not the shell".to_string(),
                format!(
                    "drive it with `session send {id}`; quit it first (/exit, q, F10; or ctrl-c, repeated while it says 'press again')"
                ),
            ));
        }
        match (s, k) {
            (SessionState::WaitingInput, Some(PromptKind::Repl)) => Some((
                "the session is at a REPL prompt, not the shell".to_string(),
                format!(
                    "send REPL lines with `session send {id} 'LINE' --enter`; leave the REPL (exit()/quit/ctrl-d) before `session run`"
                ),
            )),
            // With the prompt hook any question counts; without it an unusual shell prompt
            // may look like a question, so only refuse where it is clear.
            (SessionState::WaitingInput, Some(kind))
                if st.hooked || unfinished || matches!(kind, PromptKind::Pager | PromptKind::Password | PromptKind::Confirm) =>
            {
                Some((
                    format!("the session is waiting for input (prompt={})", format!("{kind:?}").to_lowercase()),
                    format!("answer with `session send {id} 'TEXT' --enter`, or `session send {id} --keys ctrl-c` to interrupt"),
                ))
            }
            _ if unfinished => Some((
                "previous command has not finished".to_string(),
                format!("`session wait {id}` waits for it (and returns its exit code), `session send {id} --keys ctrl-c` interrupts"),
            )),
            _ if st.hooked => Some((
                "a command started with `session send` is still running (the shell prompt is not back)".to_string(),
                format!("`session wait {id}` waits for the prompt and returns the exit code; `session send {id} --keys ctrl-c` interrupts"),
            )),
            _ => None,
        }
    }

    /// Wait for the running `run` to finish, ask for input, start a full-screen program or a
    /// nested shell, or time out.
    async fn await_run(&self, reader: &str, timeout: Duration, started: Instant) -> Result<Observation> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut prompt_settled = false;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut settle_from = None;
            'check: {
                let mut st = self.state.lock().unwrap();
                let (begun, done) = st
                    .run
                    .as_ref()
                    .map(|r| (r.begin.is_some(), r.end.is_some()))
                    .unwrap_or((false, false));
                if done && !prompt_settled && !st.closed && !st.at_marker() {
                    // Give the shell a moment to print its next prompt so the reported
                    // state is `waiting_input` rather than `running`.
                    prompt_settled = true;
                    settle_from = Some(st.total());
                    break 'check;
                }
                let timed_out = tokio::time::Instant::now() >= deadline;
                let mut nested = false;
                // A prompt for input, a full-screen program or a nested shell while running:
                // return early so the agent can look before answering.
                let waiting = !timed_out && !done && begun && started.elapsed() >= Duration::from_millis(300) && {
                    let (s, k, _) = st.state_now(Duration::from_millis(400));
                    nested = s == SessionState::WaitingInput && k == Some(PromptKind::Shell) && !st.tui();
                    // TUIs animate constantly, so they count as settled once the screen (spinners and
                    // counters aside) stops changing and no busy marker is visible.
                    (st.tui() && st.screen_changed_at.elapsed() >= Duration::from_millis(800) && !st.busy(&st.screen_text()))
                        || st.asks_input(s, k)
                };
                let gone = st.run.is_none();
                if done || timed_out || st.closed || st.detached || waiting || gone {
                    let (out, mut exit_code, mut completed) = st.take_output(reader);
                    if st.closed && !completed {
                        // e.g. `exit 3`: the shell itself ended.
                        exit_code = st.exit_code.map(i64::from);
                        completed = exit_code.is_some();
                        st.run = None;
                    }
                    if nested {
                        st.events.push(
                            "the command started a shell (sudo -i, su, ssh, docker exec...) that is now at its prompt: `session run` runs inside it, `exit` returns to the outer shell"
                                .into(),
                        );
                    }
                    let idle = if completed { Duration::from_millis(150) } else { SETTLE };
                    let mut obs = self.build(&mut st, reader, out, idle, exit_code, completed);
                    if !completed && timed_out {
                        obs.timed_out = true;
                    }
                    if !completed && obs.hint.is_none() {
                        obs.hint = Some(if st.closed {
                            "session ended while the command was running".into()
                        } else if st.detached {
                            disconnected_err(self.key(), self.persist.is_some()).hint.unwrap_or_default()
                        } else if timed_out {
                            format!(
                                "not finished after {secs}s (exit 124 = still running). `session wait {id}` keeps waiting and reports the exit code; `session send {id} --keys ctrl-c` interrupts",
                                secs = timeout.as_secs(),
                                id = self.key()
                            )
                        } else if nested {
                            format!("inside the nested shell now: `session run {} -- CMD`", self.key())
                        } else {
                            format!(
                                "waiting for input: answer with `session send {} ...`; `session wait` then reports the exit code",
                                self.key()
                            )
                        });
                    }
                    return Ok(obs);
                }
            }
            if let Some(from) = settle_from {
                self.settle(from, false, Duration::from_millis(150), Duration::from_millis(800))
                    .await;
                continue;
            }
            let now = tokio::time::Instant::now();
            let wait = deadline.saturating_duration_since(now).min(Duration::from_millis(250));
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Wait for a regex in the output produced since the last input (or the last match).
    pub async fn expect(&self, reader: &str, pattern: &str, timeout: Duration) -> Result<(Observation, Option<Vec<Option<String>>>)> {
        let re = Regex::new(pattern)?;
        let _g = self.lock_op(reader, &format!("expect {pattern}"), timeout).await?;
        self.touch();
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.state.lock().unwrap();
                let scan_from = st.last_input_at.max(st.last_expect_end).max(st.base);
                let a = st.idx(scan_from);
                let seg = st.text[a..].to_string();
                let clean = text::normalize_terminal(&seg);
                if let Some(c) = re.captures(&clean) {
                    let groups: Vec<Option<String>> = c.iter().map(|m| m.map(|m| m.as_str().to_string())).collect();
                    let end = c.get(0).unwrap().end();
                    let match_end = scan_from + raw_offset_for_clean(&seg, end) as u64;
                    // Show only what the agent has not read yet, up to the match.
                    let cursor = st.reader(reader).cursor;
                    let shown_from = cursor.max(scan_from).min(match_end);
                    let (x, y) = (st.idx(shown_from), st.idx(match_end));
                    let out = text::normalize_terminal(&st.text[x..y]);
                    st.last_expect_end = match_end;
                    let r = st.reader(reader);
                    r.cursor = r.cursor.max(match_end);
                    let obs = self.build(&mut st, reader, out, SETTLE, None, false);
                    return Ok((obs, Some(groups)));
                }
                // Fail fast when the shell is back at its prompt: nothing more will come.
                let idle_shell = started.elapsed() >= Duration::from_millis(1500)
                    && st.run.is_none()
                    && if st.hooked {
                        st.at_marker()
                    } else {
                        let (s, k, _) = st.state_now(Duration::from_millis(1500));
                        s == SessionState::WaitingInput && k == Some(PromptKind::Shell)
                    };
                if st.closed || st.detached || idle_shell || tokio::time::Instant::now() >= deadline {
                    let mut obs = self.observe(&mut st, reader, SETTLE);
                    obs.hint = Some(if idle_shell {
                        format!("pattern {pattern:?} not found since the last input, and the shell is idle at its prompt")
                    } else {
                        format!(
                            "pattern {pattern:?} not seen within {}s{}",
                            timeout.as_secs(),
                            if st.closed { " (session ended)" } else { "" }
                        )
                    });
                    return Ok((obs, None));
                }
            }
            let wait = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_millis(250));
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    pub fn screen(&self) -> ScreenSnapshot {
        self.touch();
        self.snapshot()
    }

    /// The current screen, without counting as activity.
    pub fn snapshot(&self) -> ScreenSnapshot {
        let st = self.state.lock().unwrap();
        let s = st.parser.screen();
        let (rows, cols) = s.size();
        let (cr, cc) = s.cursor_position();
        let (state, kind, _) = st.state_now(SETTLE);
        let rendered = render_screen(s);
        let rendered: Vec<&str> = rendered.lines().filter(|l| !l.contains(marker::HOOK_FN)).collect();
        let contents = text::compact_screen(&rendered.join("\n"));
        ScreenSnapshot {
            session: self.key().to_string(),
            rows,
            cols,
            cursor_row: cr,
            cursor_col: cc,
            alt_screen: st.tui(),
            state,
            prompt: kind,
            screen: text::redact(&contents, &st.redact),
        }
    }

    pub fn app_cursor(&self) -> bool {
        self.state.lock().unwrap().parser.screen().application_cursor()
    }

    /// Write raw input without waiting for a response (session setup).
    pub async fn inject(&self, s: &str) -> Result<()> {
        self.note_submit(s.as_bytes());
        self.input.write(s.as_bytes()).await
    }

    /// Encode typed text for the remote side (e.g. GBK hosts).
    pub fn encode_text(&self, s: &str) -> Vec<u8> {
        let enc = self.state.lock().unwrap().cleaner.encoding();
        text::encode_for(enc, s)
    }

    /// Text as a paste: bracketed (ESC[200~ ... ESC[201~) when the program enabled that mode,
    /// so multi-line text is inserted as-is instead of each newline acting as Enter.
    pub fn paste_bytes(&self, s: &str) -> Vec<u8> {
        let bracketed = self.state.lock().unwrap().parser.screen().bracketed_paste();
        let body = self.encode_text(s);
        if bracketed {
            [b"\x1b[200~".as_slice(), &body, b"\x1b[201~"].concat()
        } else {
            body
        }
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.clamp(20, 1000);
        let rows = rows.clamp(5, 500);
        match &self.input.backend {
            Backend::Pty => self.input.writer.window_change(cols as u32, rows as u32, 0, 0).await?,
            Backend::Tmux { .. } => self
                .input
                .writer
                .data_bytes(tmux::resize(cols, rows))
                .await
                .map_err(|e| Error::remote(e.to_string()))?,
        }
        self.state.lock().unwrap().parser.screen_mut().set_size(rows, cols);
        *self.cols.lock().unwrap() = (cols, rows);
        Ok(())
    }

    /// End the session: the remote shell exits (a persistent session's tmux session is killed).
    pub async fn close(&self) {
        if let Backend::Tmux { name, .. } = &self.input.backend {
            let _ = exec_capture(&self.conn, &sh_c(&tmux::kill_cmd(name)), Duration::from_secs(10)).await;
            let _ = tmux::remove(&self.paths, &self.id);
        }
        let _ = self.input.writer.eof().await;
        let _ = self.input.writer.close().await;
        {
            let mut st = self.state.lock().unwrap();
            st.closed = true;
            st.detached = false;
            st.flush_transcript(true);
        }
        self.notify.notify_waiters();
    }

    /// Stop using the session without ending it where possible: a persistent session detaches
    /// (tmux keeps the shell running, a later call reattaches); others close.
    pub async fn detach_or_close(&self) {
        if matches!(self.input.backend, Backend::Tmux { .. }) {
            {
                let mut st = self.state.lock().unwrap();
                st.detached = true;
                st.note("detached");
                st.flush_transcript(true);
            }
            let _ = self.input.writer.data_bytes(b"detach-client\n".to_vec()).await;
            let _ = self.input.writer.eof().await;
            let _ = self.input.writer.close().await;
            self.notify.notify_waiters();
        } else {
            self.close().await;
        }
    }
}

/// End a persistent session that is not attached: kill its tmux session and forget it.
pub async fn kill_persistent(conn: &Conn, rec: &tmux::PersistRec, paths: &Paths) -> Result<()> {
    let _ = exec_capture(conn, &sh_c(&tmux::kill_cmd(&rec.tmux)), Duration::from_secs(10)).await?;
    tmux::remove(paths, &rec.id)
}

/// `<id>-<name>.log`, so `session log <name>` finds it after close or a daemon restart.
fn transcript_file(id: &str, name: Option<&str>) -> String {
    match name.filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))) {
        Some(n) => format!("{id}-{n}.log"),
        None => format!("{id}.log"),
    }
}

/// Attach a control-mode client to a tmux session: returns the channel, the bytes that draw the
/// current screen, and whether the shell is at its prompt (no program running in the pane).
async fn attach_tmux(conn: &Conn, rec: &tmux::PersistRec) -> Result<(russh::Channel<Msg>, Option<Vec<u8>>, bool)> {
    let name = &rec.tmux;
    let (snap, code) = exec_capture(
        conn,
        &sh_c(&format!(
            "{}; {} && tmux display -p -t {} 'XSSH_CMD #{{pane_current_command}}'",
            tmux::detach_stale_cmd(name),
            tmux::snapshot_cmd(name),
            tmux::pane_target(name)
        )),
        Duration::from_secs(15),
    )
    .await?;
    if code != Some(0) {
        return Err(Error::not_found(format!("tmux session {name} no longer exists on {}", rec.host))
            .hint("its shell exited (or the host rebooted); open a new session"));
    }
    let (body, cmd) = match snap.rfind("\nXSSH_CMD ") {
        Some(i) => (&snap[..i + 1], snap[i + 10..].trim()),
        None => (snap.as_str(), ""),
    };
    let at_prompt = ["bash", "zsh", "sh", "dash", "ash", "ksh", "mksh", "busybox"].contains(&cmd);
    let seed = tmux::snapshot_to_terminal(body);
    let ch = conn.open_session().await?;
    // Through `sh`: zsh would expand `=name` (its EQUALS option), fish/csh parse differently.
    ch.exec(true, sh_c(&format!("exec tmux -C attach -t ={name}")).into_bytes()).await?;
    Ok((ch, seed, at_prompt))
}

/// The line typed for `session run`: begin marker, the command through `eval`, end marker with
/// its status. Control characters in the command (TAB would trigger completion, newlines
/// continuation prompts) travel as `printf %b` escapes, so the typed line is one plain line.
/// With `hook`, the prompt hook is installed first (a nested shell, or a hook that got lost).
fn wrapper(command: &str, nonce: &str, hook: bool) -> String {
    let cmd = command.trim_end();
    let body = if cmd.chars().any(char::is_control) {
        let mut esc = String::with_capacity(cmd.len() + 8);
        for c in cmd.chars() {
            match c {
                '\\' => esc.push_str("\\\\"),
                '\n' => esc.push_str("\\n"),
                '\t' => esc.push_str("\\t"),
                '\r' => esc.push_str("\\r"),
                c if c.is_control() && (c as u32) < 0x80 => esc.push_str(&format!("\\0{:03o}", c as u32)),
                c => esc.push(c),
            }
        }
        format!("\"$(printf '%b' {})\"", shq(&esc))
    } else {
        shq(cmd)
    };
    let prefix = if hook {
        format!("{} ", marker::setup_prefix())
    } else {
        String::new()
    };
    format!(" {prefix}printf '%s_%s\\n' __XSSH_B {nonce}; eval {body}; printf '\\n%s_%s_%s\\n' __XSSH_E {nonce} \"$?\"\n")
}

/// Right-trim every line (terminal output is often space-padded).
fn trim_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, l) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(l.trim_end());
    }
    out
}

fn closed_err(id: &str) -> Error {
    Error::new(ErrorCode::SessionClosed, format!("session {id} has exited")).hint("open a new one with `xssh session open <host>`")
}

fn disconnected_err(key: &str, persistent: bool) -> Error {
    if persistent {
        Error::new(ErrorCode::Connect, format!("session {key} is detached (connection lost)")).hint(format!(
            "its shell keeps running in tmux on the host; the next `xssh session` call on {key} reattaches"
        ))
    } else {
        Error::new(
            ErrorCode::Connect,
            format!("session {key} lost its connection; its remote shell and the programs it ran are gone"),
        )
        .hint("open a new session (`xssh session open HOST --persist` keeps the shell alive across disconnects)")
    }
}

/// Given raw (un-normalized) text and a byte offset into its normalized form,
/// find how many raw bytes produce at least that much normalized text.
fn raw_offset_for_clean(raw: &str, clean_end: usize) -> usize {
    let mut lo = 0usize;
    let mut hi = raw.len();
    while lo < hi {
        let mut mid = (lo + hi) / 2;
        while mid < raw.len() && !raw.is_char_boundary(mid) {
            mid += 1;
        }
        if text::normalize_terminal(&raw[..mid]).len() >= clean_end {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    while lo < raw.len() && !raw.is_char_boundary(lo) {
        lo += 1;
    }
    lo
}

/// Decide whether an auto-responder should fire on the current line.
fn autorespond(st: &mut State) -> Option<Vec<u8>> {
    let line = st.current_line();
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }
    let total = st.total();
    // Only fire once per prompt: require a new line since the last fill.
    let fresh = match &st.last_fill {
        Some((at, _)) => st.text[st.idx(*at)..].contains('\n'),
        None => true,
    };
    if !fresh {
        return None;
    }
    // Detect a rejected auto-filled sudo password.
    if let Some((at, kind)) = &st.last_fill
        && kind == "sudo"
        && !st.sudo_disabled
    {
        let after = st.text[st.idx(*at)..].to_ascii_lowercase();
        if after.contains("try again") || after.contains("incorrect password") || after.contains("authentication failure") {
            st.sudo_disabled = true;
            st.events
                .push("stored sudo password was rejected; auto-fill disabled for this session".into());
            return None;
        }
    }
    if !st.sudo_disabled && st.sudo_password.is_some() && prompt::is_sudo_prompt(line, st.after_sudo) {
        let mut bytes = st.sudo_password.as_ref().map(|p| p.as_bytes().to_vec()).unwrap_or_default();
        bytes.push(b'\r');
        st.last_fill = Some((total, "sudo".into()));
        // One fill per `sudo`: a later plain "Password:" (su, ssh) is not sudo's.
        st.after_sudo = false;
        st.events.push("sudo password auto-filled".into());
        return Some(bytes);
    }
    let hit = st.responders.iter().position(|r| r.re.is_match(line))?;
    let r = &st.responders[hit];
    let mut bytes = r.reply.as_bytes().to_vec();
    if r.enter {
        bytes.push(b'\r');
    }
    let label = r.label.clone();
    st.last_fill = Some((total, "rule".into()));
    st.events.push(format!("auto-responded to prompt matching {label:?}"));
    Some(bytes)
}

/// Registry of open sessions.
#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl SessionManager {
    /// Fail when a live session already uses `name` (checked before opening a remote shell).
    pub fn check_name(&self, name: Option<&str>) -> Result<()> {
        let Some(name) = name else { return Ok(()) };
        let m = self.sessions.lock().unwrap();
        if m.values().any(|x| x.name.as_deref() == Some(name) && !x.is_closed()) {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                format!("a session named '{name}' is already open"),
            )
            .hint(format!(
                "sessions are shared by all agents on this machine: reuse '{name}' only if it is yours (`xssh session list` shows owner, host and last command), else pick another --name"
            )));
        }
        Ok(())
    }

    pub fn insert(&self, s: Arc<Session>) -> Result<()> {
        self.check_name(s.name.as_deref())?;
        let mut m = self.sessions.lock().unwrap();
        if let Some(name) = &s.name {
            // An exited session keeps its name only until it is replaced.
            m.retain(|_, x| x.name.as_deref() != Some(name));
        }
        m.insert(s.id.clone(), s);
        Ok(())
    }

    /// Put a reattached session in place of its detached one.
    pub fn replace(&self, s: Arc<Session>) {
        let mut m = self.sessions.lock().unwrap();
        m.retain(|_, x| x.id != s.id && !(s.name.is_some() && x.name == s.name));
        m.insert(s.id.clone(), s);
    }

    pub fn get(&self, key: &str) -> Result<Arc<Session>> {
        let m = self.sessions.lock().unwrap();
        m.get(key)
            .cloned()
            .or_else(|| {
                let mut named: Vec<&Arc<Session>> = m.values().filter(|s| s.name.as_deref() == Some(key)).collect();
                // Prefer a live session over an exited one with the same name.
                named.sort_by_key(|s| s.is_closed());
                named.first().map(|s| (*s).clone())
            })
            .ok_or_else(|| {
                Error::not_found(format!("no session '{key}'"))
                    .hint("list sessions with `xssh session list`; sessions end when the daemon restarts unless opened with --persist")
            })
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().remove(id)
    }

    /// Also brings every transcript up to date (`session log` lists first).
    pub fn list(&self) -> Vec<SessionInfo> {
        let m = self.sessions.lock().unwrap();
        for s in m.values() {
            s.state.lock().unwrap().flush_transcript(true);
        }
        let mut v: Vec<SessionInfo> = m.values().map(|s| s.info()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        v
    }

    pub fn all(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    /// Sessions that keep the daemon alive (detached persistent ones do not: tmux holds them).
    pub fn count_open(&self) -> usize {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| {
                let st = s.state.lock().unwrap();
                !st.closed && !st.detached
            })
            .count()
    }

    /// Close sessions nobody used and that printed nothing for longer than `idle` (never one
    /// with a command still running); persistent ones detach instead. Exited ones are forgotten
    /// after a grace period.
    pub async fn gc(&self, idle: Duration) {
        let all: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        for s in all {
            let (gone, quiet_for) = {
                let st = s.state.lock().unwrap();
                (st.closed || st.detached, st.last_activity.elapsed())
            };
            if gone {
                if quiet_for > Duration::from_secs(600) {
                    self.remove(&s.id);
                }
            } else if s.idle_for() > idle && !s.run_pending() {
                s.detach_or_close().await;
                self.remove(&s.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_redraws() {
        assert_eq!(redraw_kind(b"\x1b[2J\x1b[H top"), Redraw::Full);
        assert_eq!(redraw_kind(b"\x1b[2K\x1b[1A\x1b[2K\x1b[G spinner"), Redraw::Inline);
        assert_eq!(redraw_kind(b"plain output\r\n"), Redraw::None);
        assert_eq!(redraw_kind(b"50%\r\x1b[K60%"), Redraw::None);
    }

    #[test]
    fn signature_ignores_animation() {
        let a = screen_signature("✻ Thinking… (12s · esc to interrupt)\n> ");
        let b = screen_signature("✶ Thinking… (13s · esc to interrupt)\n> ");
        let c = screen_signature("✶ Thinking… (13s · esc to interrupt)\n> hi");
        assert_eq!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn marks_highlighted_menu_item() {
        let mut p = vt100::Parser::new(5, 30, 0);
        p.process(b"  Apple\r\n  \x1b[7mBanana\x1b[0m\r\n  Cherry");
        let s = render_screen(p.screen());
        assert!(s.contains("«Banana»"), "{s}");
        assert!(s.contains("  Apple\n"));
    }

    #[test]
    fn wrapper_is_one_plain_line() {
        let w = wrapper("printf 'a\\tb\\n'", "abcdefgh", false);
        assert!(w.starts_with(" printf '%s_%s\\n' __XSSH_B abcdefgh; eval 'printf"), "{w}");
        // TAB and newlines never reach the terminal as typed characters.
        let w = wrapper("cat <<'EOF'\na\tb\nEOF", "abcdefgh", false);
        assert_eq!(w.matches('\n').count(), 1);
        assert!(!w.contains('\t'));
        assert!(w.contains("eval \"$(printf '%b' 'cat <<'\\''EOF'\\''\\na\\tb\\nEOF')\""), "{w}");
        let w = wrapper("true", "abcdefgh", true);
        assert!(w.contains(marker::HOOK_FN) && w.contains("__XSSH_B abcdefgh"));
    }

    /// A State without a channel, for exercising the stream logic.
    fn state() -> State {
        let mut st = State::new(10, 80, Some("utf-8"));
        st.last_output = Instant::now() - Duration::from_secs(5);
        st
    }

    fn start_run(st: &mut State, nonce: &str, owner: &str) {
        st.submitted = true;
        st.run = Some(RunTrack {
            nonce: nonce.into(),
            begin_marker: format!("__XSSH_B_{nonce}"),
            end_re: Regex::new(&format!(r"__XSSH_E_{nonce}_(\d+)\r?\n")).unwrap(),
            begin: None,
            end: None,
            owner: owner.into(),
        });
    }

    #[test]
    fn unusual_prompt_is_recognized_by_marker() {
        let mut st = state();
        st.ingest_bytes("\x1b]6973;P;0\x07~/src ❯ ".as_bytes());
        std::thread::sleep(Duration::from_millis(160));
        let (s, k, _) = st.state_now(SETTLE);
        assert_eq!((s, k), (SessionState::WaitingInput, Some(PromptKind::Shell)));
        // A prompt that ends in ':' (would look like a question) is still the shell.
        st.submitted = true;
        st.ingest_bytes(b"\r\n\x1b]6973;P;1\x07me:");
        assert!(st.at_marker());
        assert_eq!(st.last_prompt_exit, Some(1));
    }

    #[test]
    fn progress_bars_are_not_tuis() {
        let mut st = state();
        st.hooked = true;
        start_run(&mut st, "aaaaaaaa", "a");
        st.ingest_bytes(b"__XSSH_B_aaaaaaaa\r\n\x1b[?25l");
        for i in 0..5 {
            st.ingest_bytes(format!("layer1 {i}0%\r\nlayer2 {i}0%\r\n\x1b[2A\x1b[2K").as_bytes());
        }
        assert!(!st.tui(), "multi-line progress must stay in the output");
        st.ingest_bytes(b"error: disk full\r\n__XSSH_E_aaaaaaaa_1\r\n");
        let (out, code, done) = st.take_output("a");
        assert!(done && code == Some(1));
        assert!(out.contains("error: disk full"), "{out}");
        // An inline TUI that reads keys does count.
        let mut st = state();
        st.hooked = true;
        st.submitted = true;
        st.ingest_bytes(b"\x1b[?2004h\x1b[?25l> prompt\r\n");
        st.ingest_bytes(b"\x1b[1A\x1b[2K> p\r\n");
        st.ingest_bytes(b"\x1b[1A\x1b[2K> pr\r\n");
        assert!(st.tui());
        st.ingest_bytes(b"\x1b[?2004l\x1b[?25h\r\n\x1b]6973;P;0\x07$ ");
        assert!(!st.tui(), "the prompt marker ends inline TUI mode");
    }

    #[test]
    fn interrupt_ends_run_at_prompt_marker() {
        let mut st = state();
        st.hooked = true;
        start_run(&mut st, "bbbbbbbb", "a");
        st.ingest_bytes(b"__XSSH_B_bbbbbbbb\r\nworking\r\n");
        st.interrupted = true;
        st.ingest_bytes("^C\r\n\x1b]6973;P;130\x07❯ ".as_bytes());
        let (out, code, done) = st.take_output("a");
        assert!(done, "run closed by the prompt");
        assert_eq!(code, Some(130));
        assert!(out.contains("working") && !out.contains('❯'), "{out:?}");
    }

    #[test]
    fn nested_shell_exit_completes_inner_run() {
        let mut st = state();
        st.hooked = true;
        start_run(&mut st, "cccccccc", "a");
        st.ingest_bytes(b"__XSSH_B_cccccccc\r\nroot@h:~# ");
        // `run` inside the nested shell supersedes the outer run.
        let old = st.run.take().unwrap();
        st.stale_runs.push(old.nonce);
        start_run(&mut st, "dddddddd", "a");
        st.ingest_bytes(b"__XSSH_B_dddddddd\r\nlogout\r\n\r\n__XSSH_E_cccccccc_0\r\n\x1b]6973;P;0\x07$ ");
        let (out, code, done) = st.take_output("a");
        assert!(done && code == Some(0), "{out:?} {code:?}");
        assert!(out.contains("logout") && !out.contains("__XSSH"), "{out:?}");
        assert!(st.events.iter().any(|e| e.contains("nested shell exited")));
    }

    #[test]
    fn readers_have_their_own_cursor() {
        let mut st = state();
        st.ingest_bytes(b"one\r\n");
        let t = st.total();
        st.reader("a").cursor = t;
        st.reader("b").cursor = t;
        st.ingest_bytes(b"two\r\n");
        assert_eq!(st.take_output("a").0, "two\n");
        assert_eq!(st.take_output("b").0, "two\n");
        st.ingest_bytes(b"three\r\n");
        assert_eq!(st.take_output("a").0, "three\n");
        // A run's result is consumed only by its owner.
        start_run(&mut st, "eeeeeeee", "a");
        st.ingest_bytes(b"__XSSH_B_eeeeeeee\r\nr\r\n__XSSH_E_eeeeeeee_0\r\n");
        let (_, code, done) = st.take_output("b");
        assert!(done && code == Some(0));
        assert!(st.run.is_some());
        let (out, _, done) = st.take_output("a");
        assert!(done && out == "r", "{out:?}");
        assert!(st.run.is_none());
    }

    #[test]
    fn scrollback_counting_survives_a_full_buffer() {
        let mut st = state();
        st.reader("a");
        for i in 0..(SCROLLBACK + 100) {
            st.parser.process(format!("line {i}\r\n").as_bytes());
        }
        let first = st.take_scrolled("a");
        assert!(first.starts_with("[1000 earlier lines omitted]"), "{}", &first[..60]);
        for i in 0..5 {
            st.parser.process(format!("more {i}\r\n").as_bytes());
        }
        let next = st.take_scrolled("a");
        assert_eq!(next.lines().count(), 5, "{next}");
        assert!(st.take_scrolled("a").is_empty());
    }

    #[test]
    fn stale_markers_are_filtered() {
        let mut st = state();
        st.ingest_bytes(b"x\r\n__XSSH_E_zzzzzzzz_0\r\ny\r\n");
        st.reader("a").cursor = 0;
        assert_eq!(st.take_output("a").0, "x\ny\n");
    }
}
