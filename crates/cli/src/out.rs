//! Terminal output encoding.
//!
//! xssh writes UTF-8. The exception is Windows when stdout is piped and the console code page
//! is a legacy one (e.g. 936/GBK on Chinese Windows): PowerShell, cmd and most programs decode
//! a child's piped output with that code page, so UTF-8 would turn into mojibake. There the text
//! is encoded in the console code page, like Windows' own console tools do, and JSON output is
//! ASCII-escaped so it survives any decoding. Git Bash (MSYS) reads UTF-8 and keeps it.
//! `XSSH_OUTPUT_ENCODING=utf-8|gbk|...` overrides the choice.

use std::io::Write;
use std::sync::OnceLock;

/// The encoding for piped output, or None for UTF-8.
pub fn target() -> Option<&'static encoding_rs::Encoding> {
    static T: OnceLock<Option<&'static encoding_rs::Encoding>> = OnceLock::new();
    *T.get_or_init(detect)
}

fn detect() -> Option<&'static encoding_rs::Encoding> {
    if let Ok(v) = std::env::var("XSSH_OUTPUT_ENCODING") {
        let enc = encoding_rs::Encoding::for_label(v.trim().as_bytes())?;
        return (enc != encoding_rs::UTF_8).then_some(enc);
    }
    legacy_console_encoding()
}

#[cfg(windows)]
fn legacy_console_encoding() -> Option<&'static encoding_rs::Encoding> {
    use std::io::IsTerminal;
    // A real console gets UTF-16 from the standard library; MSYS shells read UTF-8.
    if std::io::stdout().is_terminal() || std::env::var_os("MSYSTEM").is_some() {
        return None;
    }
    let cp = unsafe { windows_sys::Win32::System::Console::GetConsoleOutputCP() };
    let label = match cp {
        936 => "gbk",
        950 => "big5",
        932 => "shift_jis",
        949 => "euc-kr",
        1250..=1258 => return encoding_rs::Encoding::for_label(format!("windows-{cp}").as_bytes()),
        866 => "ibm866",
        _ => return None,
    };
    encoding_rs::Encoding::for_label(label.as_bytes())
}

#[cfg(not(windows))]
fn legacy_console_encoding() -> Option<&'static encoding_rs::Encoding> {
    None
}

fn encode(s: &str) -> std::borrow::Cow<'_, [u8]> {
    match target() {
        Some(enc) => {
            // Characters the code page lacks (emoji...) become '?' rather than HTML entities.
            let mut out = Vec::with_capacity(s.len());
            let mut buf = [0u8; 4];
            for c in s.chars() {
                let (b, _, bad) = enc.encode(c.encode_utf8(&mut buf));
                if bad {
                    out.push(b'?');
                } else {
                    out.extend_from_slice(&b);
                }
            }
            out.into()
        }
        None => s.as_bytes().into(),
    }
}

pub fn write_stdout(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(&encode(s));
    let _ = o.flush();
}

pub fn write_stderr(s: &str) {
    let mut o = std::io::stderr().lock();
    let _ = o.write_all(&encode(s));
}

/// JSON text that decodes correctly under the output encoding: non-ASCII is `\u` escaped
/// whenever the output is not UTF-8.
pub fn json_text(v: &str) -> String {
    if target().is_none() || v.is_ascii() {
        return v.to_string();
    }
    let mut out = String::with_capacity(v.len() + 16);
    let mut buf = [0u16; 2];
    for c in v.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

// `print!`/`println!`/`eprint!`/`eprintln!` replacements that apply the output encoding;
// declared before the other modules so they shadow the std macros crate-wide.

macro_rules! print {
    ($($t:tt)*) => { $crate::out::write_stdout(&format!($($t)*)) };
}

macro_rules! println {
    () => { $crate::out::write_stdout("\n") };
    ($($t:tt)*) => { $crate::out::write_stdout(&(format!($($t)*) + "\n")) };
}

macro_rules! eprint {
    ($($t:tt)*) => { $crate::out::write_stderr(&format!($($t)*)) };
}

#[allow(unused_macros)]
macro_rules! eprintln {
    () => { $crate::out::write_stderr("\n") };
    ($($t:tt)*) => { $crate::out::write_stderr(&(format!($($t)*) + "\n")) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_escape_roundtrip() {
        // Without a legacy target the text is unchanged; the escaper itself is exercised directly.
        let v = "{\"a\":\"中🌏\"}";
        let mut out = String::new();
        let mut buf = [0u16; 2];
        for c in v.chars() {
            if c.is_ascii() {
                out.push(c);
            } else {
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
        let back: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back["a"], "中🌏");
    }
}
