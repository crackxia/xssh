//! Invisible prompt markers: an OSC sequence the shell prints right before each prompt, so the
//! session knows the shell is back at its prompt whatever PS1 looks like (starship `❯`, busybox
//! `/ # `, a prompt ending in `:`), and gets the last command's exit status with it.
//!
//! The hook is installed by typing one line into the shell: bash uses `PROMPT_COMMAND`, zsh a
//! `precmd` function, other POSIX shells (dash, busybox ash, ksh) a prefix on `PS1`. Terminals
//! ignore unknown OSC sequences, and the stream cleaner drops them from the text.

/// Start of a marker; it continues with `;<exit status>` and ends with BEL.
pub const START: &str = "\x1b]6973;P";
const END: char = '\x07';

/// Name of the shell function the hook defines (also used to hide the setup line).
pub const HOOK_FN: &str = "__xssh_p";

/// One line, typed with a leading space (kept out of history where `ignorespace` is set), that
/// installs the hook in the current shell. It is safe to type again: an installed hook is kept.
/// Shell-specific syntax sits inside `eval` so every POSIX shell can parse the line.
pub fn setup_line() -> String {
    let bash = r#"__xssh_p(){ local e=$?; printf '\033]6973;P;%s\007' "$e"; return $e; }; case "${PROMPT_COMMAND-}" in *__xssh_p*) ;; *) PROMPT_COMMAND="__xssh_p${PROMPT_COMMAND:+;$PROMPT_COMMAND}";; esac"#;
    let zsh = r#"__xssh_p(){ printf '\033]6973;P;%s\007' "$?"; }; (( ${precmd_functions[(I)__xssh_p]} )) || precmd_functions=(__xssh_p $precmd_functions)"#;
    let posix = r#"case "$PS1" in *6973*) ;; *) PS1="$(printf '\033]6973;P;')\$?$(printf '\007')$PS1";; esac"#;
    format!(
        " if [ -n \"${{BASH_VERSION-}}\" ]; then eval {}; elif [ -n \"${{ZSH_VERSION-}}\" ]; then eval {}; else eval {}; fi\n",
        sq(bash),
        sq(zsh),
        sq(posix)
    )
}

/// Same as [`setup_line`] without the newline and leading space, to prefix a command line.
pub fn setup_prefix() -> String {
    format!("{};", setup_line().trim())
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Incremental marker scanner (markers may be split across reads).
#[derive(Default)]
pub struct Scanner {
    tail: String,
}

/// A piece of decoded output: text to process, or a marker (with the exit status it carried).
#[derive(Debug, PartialEq)]
pub enum Piece {
    Text(String),
    Marker(Option<i64>),
}

impl Scanner {
    /// Split decoded output into text and markers; an incomplete marker at the end is held back.
    pub fn feed(&mut self, s: &str) -> Vec<Piece> {
        let buf = if self.tail.is_empty() {
            s.to_string()
        } else {
            let mut b = std::mem::take(&mut self.tail);
            b.push_str(s);
            b
        };
        let mut out = vec![];
        let mut at = 0;
        while let Some(p) = buf[at..].find(START) {
            let start = at + p;
            let body_from = start + START.len();
            let Some(e) = buf[body_from..].find(END) else {
                // Incomplete: keep it for the next read unless it is clearly not a marker.
                if buf.len() - start < 64 {
                    push_text(&mut out, &buf[at..start]);
                    self.tail = buf[start..].to_string();
                    return out;
                }
                break;
            };
            push_text(&mut out, &buf[at..start]);
            let body = &buf[body_from..body_from + e];
            out.push(Piece::Marker(body.strip_prefix(';').and_then(|c| c.trim().parse().ok())));
            at = body_from + e + END.len_utf8();
        }
        // A trailing prefix of START (e.g. a lone ESC) may be the start of a marker.
        let rest = &buf[at..];
        let keep = (1..START.len().min(rest.len() + 1))
            .rev()
            .find(|&n| rest.is_char_boundary(rest.len() - n) && START.starts_with(&rest[rest.len() - n..]))
            .unwrap_or(0);
        push_text(&mut out, &rest[..rest.len() - keep]);
        self.tail = rest[rest.len() - keep..].to_string();
        out
    }
}

fn push_text(out: &mut Vec<Piece>, s: &str) {
    if !s.is_empty() {
        out.push(Piece::Text(s.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_markers_across_chunks() {
        let mut s = Scanner::default();
        assert_eq!(
            s.feed("out\r\n\x1b]6973;P;0\x07user@h:~$ "),
            vec![
                Piece::Text("out\r\n".into()),
                Piece::Marker(Some(0)),
                Piece::Text("user@h:~$ ".into())
            ]
        );
        assert_eq!(s.feed("x\x1b]69"), vec![Piece::Text("x".into())]);
        assert_eq!(s.feed("73;P;130\x07❯ "), vec![Piece::Marker(Some(130)), Piece::Text("❯ ".into())]);
        // PS1-based hook where the shell did not expand `$?`.
        assert_eq!(
            s.feed("\x1b]6973;P;$?\x07/ # "),
            vec![Piece::Marker(None), Piece::Text("/ # ".into())]
        );
        // A lone ESC at the end is held back, then released as text.
        assert_eq!(s.feed("a\x1b"), vec![Piece::Text("a".into())]);
        assert_eq!(s.feed("[1m"), vec![Piece::Text("\x1b[1m".into())]);
        // Other OSC sequences pass through.
        assert_eq!(s.feed("\x1b]0;title\x07"), vec![Piece::Text("\x1b]0;title\x07".into())]);
    }

    #[test]
    fn setup_line_is_one_line() {
        let l = setup_line();
        assert!(l.starts_with(' ') && l.ends_with('\n') && l.matches('\n').count() == 1);
        assert!(l.contains(HOOK_FN));
        assert!(setup_prefix().ends_with("fi;"));
    }
}
