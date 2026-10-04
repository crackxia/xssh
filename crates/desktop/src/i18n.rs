//! Interface language: Chinese or English. The first run follows the system UI language; the
//! switch at the bottom of the sidebar changes it and remembers it in `desktop.json` in the data
//! folder. Text is written inline at each use as a (Chinese, English) pair: `t("主机", "Hosts")`,
//! or `tf!("{n} 台", "{n} hosts")` for formatted text.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static EN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// The language's own name, as the switch shows it.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
        }
    }
}

pub fn lang() -> Lang {
    if is_en() { Lang::En } else { Lang::Zh }
}

pub fn is_en() -> bool {
    EN.load(Ordering::Relaxed)
}

/// The text for the current language.
pub fn t(zh: &'static str, en: &'static str) -> &'static str {
    if is_en() { en } else { zh }
}

/// `format!` in the current language: `tf!("{n} 台", "{n} hosts")`.
macro_rules! tf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_en() { format!($en $(, $arg)*) } else { format!($zh $(, $arg)*) }
    };
}
pub(crate) use tf;

fn settings_file(home: &Path) -> PathBuf {
    home.join("desktop.json")
}

/// The saved choice, else the system language. Call before any text is built.
pub fn init(home: &Path) {
    let saved = std::fs::read_to_string(settings_file(home))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["lang"].as_str().map(String::from));
    let lang = match saved.as_deref() {
        Some("en") => Lang::En,
        Some("zh") => Lang::Zh,
        _ => system(),
    };
    apply(lang);
}

/// Switch and remember. Text built earlier (inputs, open dialogs) keeps its language until rebuilt.
pub fn set(lang: Lang, home: &Path) {
    apply(lang);
    let code = if lang == Lang::En { "en" } else { "zh" };
    let _ = std::fs::write(settings_file(home), format!("{{\"lang\":\"{code}\"}}\n"));
}

fn apply(lang: Lang) {
    EN.store(lang == Lang::En, Ordering::Relaxed);
    // Built-in component text (input context menu, pickers).
    gpui_kit::component::set_locale(if lang == Lang::En { "en" } else { "zh-CN" });
}

#[cfg(windows)]
fn system() -> Lang {
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
    // The primary language is the low 10 bits of a LANGID; 0x04 is Chinese.
    if unsafe { GetUserDefaultUILanguage() } & 0x3ff == 0x04 {
        Lang::Zh
    } else {
        Lang::En
    }
}

#[cfg(not(windows))]
fn system() -> Lang {
    let v = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    if v.is_some_and(|v| v.starts_with("zh")) {
        Lang::Zh
    } else {
        Lang::En
    }
}
