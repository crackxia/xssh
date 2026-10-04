//! Text helpers shared by exec/session/file operations: decoding, ANSI
//! stripping, terminal-overwrite normalization, truncation, redaction, quoting.

use crate::paths::Paths;
use rand::RngExt;
use std::path::PathBuf;

/// Decode remote bytes. `encoding` is a WHATWG label such as "gbk"; default UTF-8 (lossy).
pub fn decode(bytes: &[u8], encoding: Option<&str>) -> String {
    decode_detect(bytes, encoding).0
}

/// Decode remote output. With no explicit encoding, UTF-8 is expected; data that is not valid
/// UTF-8 but is valid GB18030 (GBK-era Chinese servers) is decoded as GB18030 and reported
/// as the second value so the caller can tell the agent.
pub fn decode_detect(bytes: &[u8], encoding: Option<&str>) -> (String, Option<&'static str>) {
    if let Some(label) = encoding
        && let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes())
    {
        let (s, _, _) = enc.decode(bytes);
        return (s.into_owned(), None);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), None),
        Err(_) => match looks_gb18030(bytes) {
            Some(s) => (s, Some("gbk")),
            None => (String::from_utf8_lossy(bytes).into_owned(), None),
        },
    }
}

/// Strict GB18030 decode of non-UTF-8 bytes that contain non-ASCII text.
fn looks_gb18030(bytes: &[u8]) -> Option<String> {
    if !bytes.iter().any(|b| *b >= 0x80) || bytes.contains(&0) {
        return None;
    }
    encoding_rs::GB18030
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(|s| s.into_owned())
}

/// Encode text typed into a remote program with the remote encoding.
pub fn encode_for(enc: &'static encoding_rs::Encoding, s: &str) -> Vec<u8> {
    if enc == encoding_rs::UTF_8 {
        return s.as_bytes().to_vec();
    }
    enc.encode(s).0.into_owned()
}

pub fn strip_ansi(s: &str) -> String {
    let stripped = strip_ansi_escapes::strip(s.as_bytes());
    String::from_utf8_lossy(&stripped).into_owned()
}

/// Private-use markers `StreamCleaner` emits for erase-in-line (ESC[K, ESC[2K).
pub const ERASE_TO_EOL: char = '\u{E000}';
pub const ERASE_LINE: char = '\u{E001}';

/// Apply terminal line semantics to already ANSI-stripped text:
/// CRLF -> LF, a lone CR rewinds to line start (progress bars), BS deletes a char,
/// other control characters (except tab) are dropped.
pub fn normalize_terminal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut line: Vec<char> = Vec::new();
    let mut col = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                col = 0;
            }
            '\n' => {
                out.extend(line.iter());
                out.push('\n');
                line.clear();
                col = 0;
            }
            '\u{8}' => col = col.saturating_sub(1),
            ERASE_TO_EOL => line.truncate(col),
            ERASE_LINE => {
                line.clear();
                line.resize(col, ' ');
            }
            '\t' => put(&mut line, &mut col, '\t'),
            c if c.is_control() => {}
            c => put(&mut line, &mut col, c),
        }
    }
    out.extend(line.iter());
    out
}

fn put(line: &mut Vec<char>, col: &mut usize, c: char) {
    if *col < line.len() {
        line[*col] = c;
    } else {
        line.push(c);
    }
    *col += 1;
}

/// Clean terminal output for an agent: strip ANSI, normalize line semantics.
pub fn clean_terminal(s: &str) -> String {
    normalize_terminal(&strip_ansi(s))
}

/// Clean non-PTY command output: strip ANSI (some tools colorize anyway) and CRLF.
pub fn clean_plain(s: &str) -> String {
    let s = if s.contains('\u{1b}') { strip_ansi(s) } else { s.to_string() };
    if s.contains('\r') { normalize_terminal(&s) } else { s }
}

/// Incremental decoder + ANSI escape stripper for PTY streams. Handles escape
/// sequences and multi-byte characters split across chunk boundaries. Keeps
/// `\r`, `\n`, `\t` and backspace so `normalize_terminal` can apply line semantics.
pub struct StreamCleaner {
    decoder: encoding_rs::Decoder,
    state: EscState,
    /// No encoding was configured: switch to GB18030 if the stream turns out not to be UTF-8.
    auto: bool,
    /// Set when a chunk was decoded as GBK by auto-detection (reported to the agent once).
    pub switched: Option<&'static str>,
    /// Parameters of the CSI sequence being parsed.
    csi: String,
}

#[derive(Clone, Copy, PartialEq)]
enum EscState {
    Normal,
    Esc,
    Csi,
    Str,
    StrEsc,
    Charset,
}

impl StreamCleaner {
    pub fn new(encoding: Option<&str>) -> Self {
        let enc = encoding
            .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
            .unwrap_or(encoding_rs::UTF_8);
        StreamCleaner {
            decoder: enc.new_decoder(),
            state: EscState::Normal,
            auto: encoding.is_none(),
            switched: None,
            csi: String::new(),
        }
    }

    /// The encoding currently used for this stream (also used to encode input).
    pub fn encoding(&self) -> &'static encoding_rs::Encoding {
        self.decoder.encoding()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> String {
        let decoded = self.decode(bytes);
        self.strip(&decoded)
    }

    /// Incrementally decode a chunk to UTF-8 (multi-byte characters may span chunks).
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let mut decoded = String::with_capacity(bytes.len() + 16);
        let cap = self.decoder.max_utf8_buffer_length(bytes.len()).unwrap_or(bytes.len() * 4 + 16);
        decoded.reserve(cap);
        let _ = self.decoder.decode_to_string(bytes, &mut decoded, false);
        // Per chunk, never sticky (a UTF-8 host may `cat` one GBK file): a chunk that is not
        // UTF-8 but decodes cleanly as GB18030 is shown as such.
        if self.auto
            && decoded.contains('\u{FFFD}')
            && !bytes.windows(3).any(|w| w == "\u{FFFD}".as_bytes())
            && let Some(s) = looks_gb18030(bytes)
        {
            self.decoder = encoding_rs::UTF_8.new_decoder();
            if self.switched.is_none() {
                self.switched = Some("gbk");
            }
            return s;
        }
        decoded
    }

    /// Strip escape sequences from already-decoded text.
    pub fn strip(&mut self, decoded: &str) -> String {
        let mut out = String::with_capacity(decoded.len());
        for c in decoded.chars() {
            self.state = match self.state {
                EscState::Normal => match c {
                    '\u{1b}' => EscState::Esc,
                    '\u{9b}' => EscState::Csi,
                    '\r' | '\n' | '\t' | '\u{8}' => {
                        out.push(c);
                        EscState::Normal
                    }
                    c if c.is_control() => EscState::Normal,
                    c => {
                        out.push(c);
                        EscState::Normal
                    }
                },
                EscState::Esc => match c {
                    '[' => {
                        self.csi.clear();
                        EscState::Csi
                    }
                    ']' | 'P' | 'X' | '^' | '_' => EscState::Str,
                    '(' | ')' | '*' | '+' | '#' | '%' => EscState::Charset,
                    _ => EscState::Normal,
                },
                EscState::Csi => {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        let n = self.csi.parse::<usize>().unwrap_or(1).clamp(1, 256);
                        match (c, self.csi.as_str()) {
                            // Cursor-forward is how Ink-style TUIs lay out spaces: keep them as spaces.
                            ('C', _) => out.extend(std::iter::repeat_n(' ', n)),
                            // Line edits used by progress output (`\r\x1b[K[ OK ]`), applied later
                            // by `normalize_terminal`.
                            ('K', "" | "0") => out.push(ERASE_TO_EOL),
                            ('K', "2") => out.push(ERASE_LINE),
                            ('D', _) => out.extend(std::iter::repeat_n('\u{8}', n)),
                            ('G', "" | "0" | "1") => out.push('\r'),
                            _ => {}
                        }
                        EscState::Normal
                    } else {
                        if self.csi.len() < 32 {
                            self.csi.push(c);
                        }
                        EscState::Csi
                    }
                }
                EscState::Str => match c {
                    '\u{7}' => EscState::Normal,
                    '\u{1b}' => EscState::StrEsc,
                    _ => EscState::Str,
                },
                EscState::StrEsc => {
                    if c == '\\' {
                        EscState::Normal
                    } else {
                        EscState::Str
                    }
                }
                EscState::Charset => EscState::Normal,
            };
        }
        out
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// Keep head and tail of `text` within `max` bytes; save the full text to the
/// outputs dir so the agent can grep/read it when needed.
pub fn truncate(text: String, max: usize, paths: Option<&Paths>, label: &str) -> Truncated {
    let total = text.len();
    if max == 0 || total <= max {
        return Truncated {
            text,
            truncated: false,
            total_bytes: total,
            full_output_path: None,
        };
    }
    let full_path = paths.and_then(|p| save_output(p, label, &text));
    let head_len = floor_char(&text, max * 2 / 5);
    let tail_start = ceil_char(&text, total - (max - head_len));
    let omitted = tail_start - head_len;
    let note = match &full_path {
        Some(p) => format!("\n\n... [xssh: {omitted} bytes omitted; full output saved to {p}] ...\n\n"),
        None => format!("\n\n... [xssh: {omitted} bytes omitted] ...\n\n"),
    };
    let mut t = String::with_capacity(max + note.len());
    t.push_str(&text[..head_len]);
    t.push_str(&note);
    t.push_str(&text[tail_start..]);
    Truncated {
        text: t,
        truncated: true,
        total_bytes: total,
        full_output_path: full_path,
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn save_output(paths: &Paths, label: &str, text: &str) -> Option<String> {
    let name = format!(
        "{}-{}-{}.txt",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        sanitize(label),
        short_id(4)
    );
    let p: PathBuf = paths.outputs_dir().join(name);
    std::fs::write(&p, text).ok()?;
    Some(p.display().to_string())
}

/// Remove saved outputs older than `hours`.
pub fn gc_outputs(paths: &Paths, hours: u64) {
    let Ok(rd) = std::fs::read_dir(paths.outputs_dir()) else { return };
    for e in rd.flatten() {
        if let Ok(m) = e.metadata()
            && m.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|d| d.as_secs() > hours * 3600)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .take(40)
        .collect()
}

/// Replace every occurrence of the given secret values with `******`.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if s.len() >= 3 && out.contains(s.as_str()) {
            out = out.replace(s.as_str(), "******");
        }
    }
    out
}

/// Quote a remote path, keeping a leading `~` / `~/` as the remote `$HOME`.
pub fn shq_path(p: &str) -> String {
    if p == "~" {
        "\"$HOME\"".into()
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("\"$HOME\"/{}", shq(rest))
    } else {
        shq(p)
    }
}

/// POSIX single-quote a string for the remote shell.
pub fn shq(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Random lowercase alphanumeric id.
pub fn short_id(len: usize) -> String {
    const CS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..len).map(|_| CS[rng.random_range(0..CS.len())] as char).collect()
}

/// Parse durations like "30s", "5m", "1h30m", or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    const MAX_SECS: f64 = 7.0 * 24.0 * 3600.0;
    let d = match s.parse::<f64>() {
        Ok(n) if n.is_finite() => std::time::Duration::from_secs_f64(n.clamp(0.0, MAX_SECS)),
        Ok(_) => return Err(format!("invalid duration '{s}'")),
        Err(_) => humantime::parse_duration(s).map_err(|e| format!("invalid duration '{s}': {e}"))?,
    };
    if d.as_secs_f64() > MAX_SECS {
        return Err(format!("duration '{s}' is longer than 7 days"));
    }
    Ok(d)
}

/// Shorten runs of one repeated rule/box character (`────`, `====`, `----`) to 8 characters.
/// Spaces, letters and digits are never shortened: indentation and column alignment must stay
/// exact (code in an editor, tables).
fn squeeze_runs(l: &str) -> String {
    let mut out = String::with_capacity(l.len());
    let mut prev = None;
    let mut run = 0usize;
    for c in l.chars() {
        if Some(c) == prev {
            run += 1;
        } else {
            prev = Some(c);
            run = 1;
        }
        if run <= 8 || c.is_whitespace() || c.is_alphanumeric() {
            out.push(c);
        }
    }
    out
}

/// Make a rendered terminal screen cheap to read: strip trailing spaces, shorten long separator
/// rules, drop leading/trailing blank rows, and collapse runs of identical rows (e.g. vim's `~`).
pub fn compact_screen(s: &str) -> String {
    let owned: Vec<String> = s.lines().map(|l| squeeze_runs(l.trim_end())).collect();
    let first = owned.iter().position(|l| !l.is_empty()).unwrap_or(owned.len());
    let owned = &owned[first..];
    let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
    let end = lines.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(0);
    let mut out = String::new();
    let mut i = 0;
    while i < end {
        let mut j = i + 1;
        while j < end && lines[j] == lines[i] {
            j += 1;
        }
        let run = j - i;
        if run >= 4 {
            out.push_str(lines[i]);
            out.push_str(&format!("   [× {run} identical rows]\n"));
        } else {
            for l in &lines[i..j] {
                out.push_str(l);
                out.push('\n');
            }
        }
        i = j;
    }
    out
}

/// Last non-empty line of text (used for prompt detection).
pub fn last_line(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r']).rsplit('\n').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_progress_and_crlf() {
        assert_eq!(normalize_terminal("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(normalize_terminal("10%\r50%\r100%\ndone"), "100%\ndone");
        assert_eq!(normalize_terminal("abc\u{8}\u{8}X\n"), "aXc\n");
        let mut sc = StreamCleaner::new(None);
        let raw = sc.strip("Upgrading nginx\r\u{1b}[K[ OK ]\nabc\r\u{1b}[2Kxy\n50%\u{1b}[3D100%\n");
        assert_eq!(normalize_terminal(&raw), "[ OK ]\nxy\n100%\n");
    }

    #[test]
    fn clean_strips_ansi() {
        assert_eq!(clean_terminal("\u{1b}[1;32mok\u{1b}[0m\r\n"), "ok\n");
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let text = (0..1000).map(|i| format!("line{i}\n")).collect::<String>();
        let t = truncate(text.clone(), 200, None, "x");
        assert!(t.truncated);
        assert!(t.text.starts_with("line0\n"));
        assert!(t.text.ends_with("line999\n"));
        assert_eq!(t.total_bytes, text.len());
        let small = truncate("hi".into(), 200, None, "x");
        assert!(!small.truncated);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let text = "你好世界".repeat(100);
        let t = truncate(text, 50, None, "x");
        assert!(t.truncated);
    }

    #[test]
    fn quoting() {
        assert_eq!(shq("abc"), "abc");
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("it's"), "'it'\\''s'");
        assert_eq!(shq(""), "''");
    }

    #[test]
    fn stream_cleaner_handles_split_sequences() {
        let mut c = StreamCleaner::new(None);
        let mut out = String::new();
        let data = "\u{1b}[1;32mgreen\u{1b}[0m \u{1b}]0;title\u{7}你好\r\n".as_bytes();
        for chunk in data.chunks(3) {
            out.push_str(&c.feed(chunk));
        }
        assert_eq!(out, "green 你好\r\n");
    }

    #[test]
    fn screen_compaction() {
        let s = "text   \n~\n~\n~\n~\n~\nstatus  \n\n\n";
        assert_eq!(compact_screen(s), "text\n~   [× 5 identical rows]\nstatus\n");
        // Indentation and alignment survive; separator rules are shortened.
        let code = "def f():\n            return 1\nname        size\n────────────────────";
        assert_eq!(compact_screen(code), "def f():\n            return 1\nname        size\n────────\n");
    }

    #[test]
    fn redaction() {
        assert_eq!(redact("pw=hunter2!", &["hunter2".into()]), "pw=******!");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30").unwrap().as_secs(), 30);
        assert_eq!(parse_duration("5m").unwrap().as_secs(), 300);
        assert_eq!(parse_duration("0.5").unwrap().as_millis(), 500);
    }
}
