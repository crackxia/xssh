//! Heuristics that classify the current terminal line, so the agent knows
//! whether the remote side is waiting for input and what kind.

use regex::Regex;
use std::sync::LazyLock;
pub use xssh_core::api::PromptKind;

// Whole words only: "Stopping services:" or "Mapping ports:" are not PIN prompts.
static PASSWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\b(password|passphrase|passcode|pin|otp)\b|密码|口令)[^\n]{0,60}[:：]\s*$").unwrap());
static CONFIRM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(\[y/n\]|\(y/n\)|\[yes/no\]|\(yes/no\)|\(yes/no/\[fingerprint\]\)|\[y/n/q\]|\by/n\b|continue\?|proceed\?|are you sure[^\n]*\?)\s*:?\s*$")
        .unwrap()
});
static PAGER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^:\s*$|\(END\)\s*$|--More--|^lines \d+-\d+)").unwrap());
static REPL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^(>>>|\.\.\.) ?$|^In \[\d+\]: ?$|^[\w-]*(sql|mysql|mariadb|sqlite|redis[^>]*|mongo[^>]*)> ?$|^\w+=[#>] ?$|^irb\([^)]*\)[^>]*> ?$|^> ?$)")
        .unwrap()
});
// Fallback only: sessions normally know they are at the prompt from the prompt marker. Shapes are
// anchored at the start of the line, so output such as "upload to bob@web1 42%" is not a prompt.
static SHELL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(\([^)]*\)\s*)*(",
        r"[\w.-]+@[\w.-]+(:[^\s]*|\s+[^\s]+)?\s?[$#%>]", // user@host:~$  user@host dir %
        r"|\[[^\]]+\][$#]",                              // [root@centos ~]#
        r"|[\w.~/:+-]{0,80}\s?[$#]",                     // /srv/app$  / #  bash-5.2$  $
        r"|[\w.-]+% ",                                   // zsh "host% " (not "100%")
        r"|% ",                                          // bare "% "
        r"|PS [^>]+>",                                   // PowerShell
        r"|[^\s]{0,40}\s?[❯›»λ]",                        // starship, pure, ...
        r"|➜\s+[^\s]+(\s+git:\([^)]*\))?(\s+✗)?",        // oh-my-zsh robbyrussell
        r") ?$"
    ))
    .unwrap()
});
// sudo "[sudo] password for bob:", sudo-rs "[sudo: authenticate] Password:", doas "doas (bob@host) password:".
static SUDO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\[sudo(: [^\]]+)?\] password( for [^:]+)?|doas \([^)]+\) password):\s*$").unwrap());
static PLAIN_PASSWORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^password:\s*$").unwrap());

/// Classify the line the cursor is on.
pub fn classify(line: &str) -> Option<PromptKind> {
    let l = line.trim_start();
    if l.is_empty() {
        return None;
    }
    if PASSWORD.is_match(l) {
        return Some(PromptKind::Password);
    }
    if CONFIRM.is_match(l) {
        return Some(PromptKind::Confirm);
    }
    if PAGER.is_match(l) {
        return Some(PromptKind::Pager);
    }
    if REPL.is_match(l) {
        return Some(PromptKind::Repl);
    }
    if SHELL.is_match(l) {
        return Some(PromptKind::Shell);
    }
    let t = l.trim_end();
    if t.ends_with(':') || t.ends_with('?') || t.ends_with('：') || t.ends_with('？') {
        return Some(PromptKind::Input);
    }
    None
}

/// Is this line a sudo password prompt? `after_sudo` is true when the agent's
/// last input mentioned sudo (macOS/BSD sudo just prints "Password:").
pub fn is_sudo_prompt(line: &str, after_sudo: bool) -> bool {
    let l = line.trim();
    SUDO.is_match(l) || (after_sudo && PLAIN_PASSWORD.is_match(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_prompts() {
        assert_eq!(classify("[sudo] password for bob: "), Some(PromptKind::Password));
        assert_eq!(classify("Enter passphrase for key '/x': "), Some(PromptKind::Password));
        assert_eq!(classify("Do you want to continue? [Y/n] "), Some(PromptKind::Confirm));
        assert_eq!(
            classify("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            Some(PromptKind::Confirm)
        );
        assert_eq!(classify("bob@web1:~$ "), Some(PromptKind::Shell));
        assert_eq!(classify("root@web1:/etc# "), Some(PromptKind::Shell));
        assert_eq!(classify("[root@centos ~]# "), Some(PromptKind::Shell));
        assert_eq!(classify("$ "), Some(PromptKind::Shell));
        assert_eq!(classify(">>> "), Some(PromptKind::Repl));
        assert_eq!(classify("mysql> "), Some(PromptKind::Repl));
        assert_eq!(classify("postgres=# "), Some(PromptKind::Repl));
        assert_eq!(classify("(END)"), Some(PromptKind::Pager));
        assert_eq!(classify("Enter your name: "), Some(PromptKind::Input));
        assert_eq!(classify("Compiling foo v0.1.0"), None);
        assert_eq!(classify("100%"), None);
    }

    #[test]
    fn no_false_prompts() {
        // B9: words that merely contain "pin".
        assert_eq!(classify("Stopping services:"), Some(PromptKind::Input));
        assert_ne!(classify("Mapping ports:"), Some(PromptKind::Password));
        assert_eq!(classify("Enter PIN for 'token':"), Some(PromptKind::Password));
        assert_eq!(classify("Verification code (OTP): "), Some(PromptKind::Password));
        // B10: user@host in the middle of progress output.
        assert_eq!(classify("upload to bob@web1 42%"), None);
        assert_ne!(classify("copying to root@db1:/tmp >"), Some(PromptKind::Shell));
        assert_eq!(classify("Total: 100$"), None);
    }

    #[test]
    fn unusual_shell_prompts() {
        for p in [
            "/ # ",
            "~ # ",
            "bash-5.2$ ",
            "~/src ❯ ",
            "❯ ",
            "➜  ~ ",
            "➜  xssh git:(main) ✗ ",
            "(venv) bob@web1:~/app$ ",
            "host% ",
            "PS C:\\Users\\bob> ",
        ] {
            assert_eq!(classify(p), Some(PromptKind::Shell), "{p:?}");
        }
    }

    #[test]
    fn sudo_prompt() {
        assert!(is_sudo_prompt("[sudo] password for bob: ", false));
        assert!(is_sudo_prompt("[sudo: authenticate] Password: ", false));
        assert!(is_sudo_prompt("doas (bob@web1) password: ", false));
        assert_eq!(classify("[sudo: authenticate] Password: "), Some(PromptKind::Password));
        assert!(is_sudo_prompt("Password:", true));
        assert!(!is_sudo_prompt("Password:", false));
    }
}
