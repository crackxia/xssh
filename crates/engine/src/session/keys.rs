//! Named key -> byte sequence mapping for `session send --keys`.

use xssh_core::error::{Error, Result};

/// Translate a comma separated key list such as `ctrl-c,up,enter`.
/// `app_cursor` selects SS3 arrow sequences (used by vim, less, ...).
pub fn encode(spec: &str, app_cursor: bool) -> Result<Vec<u8>> {
    Ok(encode_each(spec, app_cursor)?.concat())
}

/// Like [`encode`], one entry per key press: TUIs may merge keys that arrive in one read
/// (two ctrl-c become one), so they are written separately.
pub fn encode_each(spec: &str, app_cursor: bool) -> Result<Vec<Vec<u8>>> {
    let mut out = vec![];
    for raw in split_spec(spec).iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        // `NAME*N` repeats; a `*` that is not followed by digits is part of the name (`*` itself).
        let (name, times) = match raw.rsplit_once('*') {
            Some((n, t)) if !n.is_empty() && !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) => (
                n,
                t.parse::<usize>().map_err(|_| Error::usage(format!("bad repeat in key '{raw}'")))?,
            ),
            _ => (raw, 1),
        };
        // A single character is sent literally (e.g. `q`, `i`, `:`, `G`, `*`, `\,`).
        let seq = if name.chars().count() == 1 {
            name.as_bytes().to_vec()
        } else {
            key(&name.to_ascii_lowercase(), app_cursor).ok_or_else(|| Error::usage(format!("unknown key '{name}'")).hint(KEY_HELP))?
        };
        for _ in 0..times.min(1000) {
            out.push(seq.clone());
        }
    }
    Ok(out)
}

/// Split on commas; `\,` is a literal comma and `\\` a backslash.
fn split_spec(spec: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = spec.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n @ (',' | '\\')) => parts.last_mut().unwrap().push(n),
                Some(n) => {
                    parts.last_mut().unwrap().push('\\');
                    parts.last_mut().unwrap().push(n);
                }
                None => parts.last_mut().unwrap().push('\\'),
            },
            ',' => parts.push(String::new()),
            c => parts.last_mut().unwrap().push(c),
        }
    }
    parts
}

pub const KEY_HELP: &str = "keys: any single character (q, i, G, :, *), enter, tab, esc, space, backspace, delete, insert, up, down, left, right, home, end, \
pgup, pgdn, f1-f12, shift-tab, ctrl-a..ctrl-z, alt-x, alt-enter, ctrl-[, ctrl-], ctrl-\\, ctrl-@, comma (or \\,), star, backslash; \
modifiers on navigation keys: shift-up, ctrl-left, alt-right, ctrl-shift-end, ...; repeat with '*N' (e.g. down*3)";

/// xterm modifier parameter for `shift-`/`alt-`/`ctrl-` prefixes on a navigation key.
fn modified(name: &str) -> Option<(u8, &str)> {
    let mut rest = name;
    let (mut shift, mut alt, mut ctrl) = (false, false, false);
    loop {
        if let Some(r) = rest.strip_prefix("shift-") {
            shift = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("alt-").or_else(|| rest.strip_prefix("meta-")) {
            alt = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("ctrl-").or_else(|| rest.strip_prefix("c-")) {
            ctrl = true;
            rest = r;
        } else {
            break;
        }
    }
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    (m > 1).then_some((m, rest))
}

fn key(name: &str, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: u8| if app_cursor { vec![0x1b, b'O', c] } else { vec![0x1b, b'[', c] };
    // Modified navigation keys: CSI 1;<m>A (arrows, home, end), CSI <n>;<m>~ (pgup, delete, ...).
    if let Some((m, base)) = modified(name) {
        let letter = match base {
            "up" => Some('A'),
            "down" => Some('B'),
            "right" => Some('C'),
            "left" => Some('D'),
            "home" => Some('H'),
            "end" => Some('F'),
            _ => None,
        };
        if let Some(l) = letter {
            return Some(format!("\x1b[1;{m}{l}").into_bytes());
        }
        let tilde = match base {
            "insert" | "ins" => Some(2),
            "delete" | "del" => Some(3),
            "pgup" | "pageup" => Some(5),
            "pgdn" | "pagedown" => Some(6),
            _ => None,
        };
        if let Some(n) = tilde {
            return Some(format!("\x1b[{n};{m}~").into_bytes());
        }
    }
    Some(match name {
        "comma" => b",".to_vec(),
        "star" | "asterisk" => b"*".to_vec(),
        "backslash" => b"\\".to_vec(),
        "enter" | "return" | "cr" => b"\r".to_vec(),
        "lf" | "newline" => b"\n".to_vec(),
        "tab" => b"\t".to_vec(),
        "shift-tab" | "backtab" => b"\x1b[Z".to_vec(),
        "esc" | "escape" => b"\x1b".to_vec(),
        "space" => b" ".to_vec(),
        "backspace" | "bs" => b"\x7f".to_vec(),
        "delete" | "del" => b"\x1b[3~".to_vec(),
        "insert" | "ins" => b"\x1b[2~".to_vec(),
        "up" => arrow(b'A'),
        "down" => arrow(b'B'),
        "right" => arrow(b'C'),
        "left" => arrow(b'D'),
        "home" => {
            if app_cursor {
                b"\x1bOH".to_vec()
            } else {
                b"\x1b[H".to_vec()
            }
        }
        "end" => {
            if app_cursor {
                b"\x1bOF".to_vec()
            } else {
                b"\x1b[F".to_vec()
            }
        }
        "pgup" | "pageup" => b"\x1b[5~".to_vec(),
        "pgdn" | "pagedown" => b"\x1b[6~".to_vec(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" => b"\x1b[24~".to_vec(),
        "ctrl-@" | "ctrl-space" => vec![0],
        "ctrl-[" => vec![0x1b],
        "ctrl-\\" => vec![0x1c],
        "ctrl-]" => vec![0x1d],
        "ctrl-^" => vec![0x1e],
        "ctrl-_" => vec![0x1f],
        "alt-enter" | "meta-enter" => b"\x1b\r".to_vec(),
        n if n.starts_with("alt-") && n.len() == 5 => vec![0x1b, n.as_bytes()[4]],
        n => {
            let c = n.strip_prefix("ctrl-").or_else(|| n.strip_prefix("c-"))?;
            let b = c.as_bytes();
            if b.len() == 1 && b[0].is_ascii_lowercase() {
                vec![b[0] - b'a' + 1]
            } else {
                return None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_keys() {
        assert_eq!(encode("ctrl-c", false).unwrap(), vec![3]);
        assert_eq!(encode("up,enter", false).unwrap(), b"\x1b[A\r".to_vec());
        assert_eq!(encode("up", true).unwrap(), b"\x1bOA".to_vec());
        assert_eq!(encode("down*3", false).unwrap(), b"\x1b[B\x1b[B\x1b[B".to_vec());
        assert!(encode("bogus", false).is_err());
        assert_eq!(encode("q", false).unwrap(), b"q".to_vec());
        assert_eq!(encode("G,esc", false).unwrap(), b"G\x1b".to_vec());
    }

    #[test]
    fn literal_comma_star_and_modifiers() {
        assert_eq!(encode("a,\\,,b", false).unwrap(), b"a,b".to_vec());
        assert_eq!(encode("comma,star", false).unwrap(), b",*".to_vec());
        assert_eq!(encode("*", false).unwrap(), b"*".to_vec());
        assert_eq!(encode("**2", false).unwrap(), b"**".to_vec());
        assert_eq!(encode("\\\\", false).unwrap(), b"\\".to_vec());
        assert_eq!(encode("shift-up", false).unwrap(), b"\x1b[1;2A".to_vec());
        assert_eq!(encode("ctrl-left", true).unwrap(), b"\x1b[1;5D".to_vec());
        assert_eq!(encode("alt-right", false).unwrap(), b"\x1b[1;3C".to_vec());
        assert_eq!(encode("ctrl-shift-end", false).unwrap(), b"\x1b[1;6F".to_vec());
        assert_eq!(encode("ctrl-delete", false).unwrap(), b"\x1b[3;5~".to_vec());
        // Plain ctrl/alt letters are unchanged.
        assert_eq!(encode("ctrl-c,alt-x", false).unwrap(), b"\x03\x1bx".to_vec());
    }
}
