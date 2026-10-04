//! Authentication chain: configured keys (with OpenSSH certificates) -> ssh-agent/Pageant ->
//! default keys -> password -> keyboard-interactive (password prompts only).

use super::keys;
use super::{ClientHandler, Ctx};
use regex::Regex;
use russh::MethodKind;
use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{Certificate, PrivateKey, PrivateKeyWithHashAlg};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use xssh_core::error::{Error, Result};
use xssh_store::hosts::Host;
use xssh_store::secrets;

type H = Handle<ClientHandler>;

/// Agent identities tried at most (servers usually disconnect after ~6 failed keys).
const AGENT_MAX: usize = 6;

/// A hidden keyboard-interactive prompt the stored password may answer.
static PASSWORD_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)pass(word|phrase)?|密码|口令|contraseña|mot de passe|kennwort|пароль").unwrap());
/// Second-factor prompts: never answered with the password (that burns attempts / locks accounts).
static OTP_PROMPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)one[- ]?time|\botp\b|verification|\bcode\b|token|2fa|mfa|authenticator|duo|yubikey|passcode|pin\b|验证码|动态")
        .unwrap()
});

#[derive(Default)]
struct Attempts {
    tried: Vec<String>,
    notes: Vec<String>,
    remaining: Option<Vec<MethodKind>>,
}

impl Attempts {
    fn allowed(&self, m: MethodKind) -> bool {
        self.remaining.as_ref().is_none_or(|r| r.contains(&m))
    }
}

/// Returns a short description of the method that succeeded.
pub async fn authenticate(ctx: &Ctx, host: &Host, handle: &mut H) -> Result<String> {
    let user = host.user.clone();
    let mut a = Attempts::default();

    // 1. Configured keys (xssh-managed key, key file, extra IdentityFile lines).
    let configured = configured_keys(ctx, host)?;
    let explicit = !configured.is_empty() || host.key_name.is_some() || host.key.is_some();
    for (label, key, cert) in configured {
        if !a.allowed(MethodKind::PublicKey) {
            break;
        }
        if let Some(r) = try_key(handle, &user, key, cert, &label, &mut a).await? {
            return Ok(r);
        }
    }
    // With configured keys, the agent and default keys are only used when ssh_config said
    // IdentitiesOnly=no (the OpenSSH default for imported hosts).
    let fallback = !explicit || host.identities_only == Some(false);

    // 2. ssh-agent / Pageant.
    if fallback
        && ctx.config.use_agent
        && a.allowed(MethodKind::PublicKey)
        && let Some(true) = try_agent(handle, &user, &mut a).await
    {
        return Ok("publickey(agent)".into());
    }

    // 3. Default keys in ~/.ssh (unencrypted only; encrypted ones need the agent).
    if fallback
        && ctx.config.use_default_keys
        && a.allowed(MethodKind::PublicKey)
        && let Some(home) = std::env::home_dir()
    {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            let p = home.join(".ssh").join(name);
            if !p.exists() {
                continue;
            }
            let key = match russh::keys::load_secret_key(&p, None) {
                Ok(k) => k,
                Err(e) => {
                    let why = if format!("{e}").to_lowercase().contains("encrypt") || encrypted_file(&p) {
                        "passphrase-protected: add it to ssh-agent, or `xssh host edit <alias> --key` + `host set-password <alias> --passphrase`"
                    } else {
                        "unreadable"
                    };
                    a.notes.push(format!("skipped ~/.ssh/{name} ({why})"));
                    continue;
                }
            };
            let cert = cert_for(&p, None);
            if let Some(r) = try_key(handle, &user, key, cert, &format!("~/.ssh/{name}"), &mut a).await? {
                return Ok(r);
            }
            if !a.allowed(MethodKind::PublicKey) {
                break;
            }
        }
    }

    // 4/5. Password, then keyboard-interactive answering password prompts with it.
    let password = if host.ad_hoc {
        None
    } else {
        ctx.secrets.get(&secrets::host_password(&host.alias))?
    };
    if let Some(pw) = &password {
        if a.allowed(MethodKind::Password) {
            let r = handle.authenticate_password(user.clone(), pw.as_str()).await?;
            a.tried.push("password".into());
            if r.success() {
                return Ok("password".into());
            }
            a.remaining = remaining_of(&r);
        }
        if a.allowed(MethodKind::KeyboardInteractive) {
            a.tried.push("keyboard-interactive".into());
            match try_keyboard_interactive(handle, &user, pw.as_str()).await? {
                Kbd::Success => return Ok("keyboard-interactive".into()),
                Kbd::Failed => {}
                Kbd::Unanswerable(prompt) => {
                    return Err(Error::auth(format!(
                        "{}@{}:{} asks for \"{prompt}\" (a second factor / one-time code); xssh answers only password prompts",
                        host.user, host.host, host.port
                    ))
                    .hint(format!(
                        "use key auth instead (`xssh key gen k && xssh key deploy {a} k` from a working login, or add the user's key with `xssh host edit {a} --key PATH`); do not guess codes",
                        a = host.alias
                    )));
                }
            }
        }
    }

    let methods = a
        .remaining
        .map(|r| r.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>().join(","))
        .unwrap_or_else(|| "unknown".into());
    let hint = if host.ad_hoc {
        format!(
            "'{}' is not a saved host, so only keys/agent are tried; save it to store a password: `xssh host add NAME --host {} --port {} --user {} --ask-password`",
            host.alias, host.host, host.port, host.user
        )
    } else if password.is_none() && !explicit {
        format!(
            "no credentials configured: store a password with `xssh host set-password {a}` (ask the user for it), \
             or configure a key with `xssh host edit {a} --key <path>`",
            a = host.alias
        )
    } else {
        format!(
            "credentials were rejected; ask the user to confirm them and update with `xssh host set-password {}`",
            host.alias
        )
    };
    // Keys on this machine that were not offered: the usual reason a key login fails.
    let mut hint = hint;
    let others = untried_keys(ctx, &a.tried);
    if !others.is_empty() {
        hint.push_str(&format!(
            "; untried private keys here: {} (pass one to `host add`/`host edit` as --key PATH, key:K as --key-name K)",
            others.join(", ")
        ));
    }
    let notes = if a.notes.is_empty() {
        String::new()
    } else {
        format!("; {}", a.notes.join("; "))
    };
    Err(Error::auth(format!(
        "authentication failed for {}@{}:{} (tried: {}; server allows: {methods}{notes})",
        host.user,
        host.host,
        host.port,
        if a.tried.is_empty() { "nothing".into() } else { a.tried.join(", ") }
    ))
    .hint(hint))
}

/// Private key files in ~/.ssh and xssh-managed keys (`key:NAME`) not among `tried`, at most 8.
fn untried_keys(ctx: &Ctx, tried: &[String]) -> Vec<String> {
    let is_private = |p: &Path| {
        let mut head = [0u8; 64];
        std::fs::File::open(p)
            .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
            .is_ok_and(|n| String::from_utf8_lossy(&head[..n]).contains("PRIVATE KEY"))
    };
    let tried_any = |file: &str| tried.iter().any(|t| t.contains(file));
    let mut out = vec![];
    if let Some(home) = std::env::home_dir()
        && let Ok(rd) = std::fs::read_dir(home.join(".ssh"))
    {
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_file() && is_private(&e.path()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !tried_any(n))
            .map(|n| format!("~/.ssh/{n}"))
            .collect();
        names.sort();
        out.extend(names);
    }
    if let Ok(rd) = std::fs::read_dir(ctx.paths.keys_dir()) {
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().extension().is_none() && is_private(&e.path()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !tried_any(n))
            .map(|n| format!("key:{n}"))
            .collect();
        names.sort();
        out.extend(names);
    }
    out.truncate(8);
    out
}

fn remaining_of(r: &AuthResult) -> Option<Vec<MethodKind>> {
    match r {
        AuthResult::Failure { remaining_methods, .. } => Some(remaining_methods.iter().copied().collect()),
        AuthResult::Success => None,
    }
}

fn encrypted_file(p: &Path) -> bool {
    std::fs::read_to_string(p).is_ok_and(|s| s.contains("ENCRYPTED") || s.contains("aes256-ctr") || s.contains("bcrypt"))
}

/// The OpenSSH certificate for a key: the configured CertificateFile, else `<key>-cert.pub`.
fn cert_for(key_path: &Path, configured: Option<&str>) -> Option<Certificate> {
    let candidates: Vec<PathBuf> = match configured {
        Some(c) => vec![PathBuf::from(c)],
        None => vec![PathBuf::from(format!("{}-cert.pub", key_path.display()))],
    };
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .find_map(|p| russh::keys::load_openssh_certificate(&p).ok())
}

type Configured = Vec<(String, PrivateKey, Option<Certificate>)>;

fn configured_keys(ctx: &Ctx, host: &Host) -> Result<Configured> {
    let mut out: Configured = vec![];
    if let Some(name) = &host.key_name {
        let key = keys::load(&ctx.paths, &ctx.secrets, name)?;
        let cert = cert_for(&keys::private_path(&ctx.paths, name), host.certificate.as_deref());
        out.push(("configured".into(), key, cert));
    }
    let pass = if host.ad_hoc {
        None
    } else {
        ctx.secrets.get(&secrets::host_passphrase(&host.alias))?
    };
    let files = host.key.iter().chain(host.identity_files.iter());
    for (i, path) in files.enumerate() {
        let is_primary = i == 0 && host.key.is_some();
        match russh::keys::load_secret_key(path, pass.as_ref().map(|p| p.as_str())) {
            Ok(k) => {
                let cert = cert_for(
                    Path::new(path),
                    if is_primary || out.is_empty() {
                        host.certificate.as_deref()
                    } else {
                        None
                    },
                );
                out.push((if is_primary { "configured".into() } else { path.clone() }, k, cert));
            }
            // The primary configured key must load; extra IdentityFile lines are best effort.
            Err(e) if is_primary && host.identity_files.is_empty() => {
                let err = Error::auth(format!("cannot load key {path}: {e}"));
                return Err(if pass.is_none() {
                    err.hint(format!(
                        "if the key is passphrase-protected, store the passphrase with `xssh host set-password {} --passphrase`",
                        host.alias
                    ))
                } else {
                    err
                });
            }
            Err(_) => {}
        }
    }
    Ok(out)
}

/// Try one key (its certificate first). Some(method) on success.
async fn try_key(
    handle: &mut H,
    user: &str,
    key: PrivateKey,
    cert: Option<Certificate>,
    label: &str,
    a: &mut Attempts,
) -> Result<Option<String>> {
    let key = Arc::new(key);
    if let Some(cert) = cert {
        a.tried.push(format!("publickey-cert({label})"));
        match handle.authenticate_openssh_cert(user.to_string(), key.clone(), cert).await {
            Ok(r) if r.success() => return Ok(Some(format!("publickey-cert({label})"))),
            Ok(r) => a.remaining = remaining_of(&r),
            Err(e) => a.notes.push(format!("certificate for {label} failed: {e}")),
        }
        if !a.allowed(MethodKind::PublicKey) {
            return Ok(None);
        }
    }
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    let r = handle
        .authenticate_publickey(user.to_string(), PrivateKeyWithHashAlg::new(key, hash))
        .await?;
    a.tried.push(format!("publickey({label})"));
    if r.success() {
        return Ok(Some(if label == "configured" {
            "publickey".into()
        } else {
            format!("publickey({label})")
        }));
    }
    a.remaining = remaining_of(&r);
    Ok(None)
}

enum Kbd {
    Success,
    Failed,
    /// A hidden prompt that is not a password prompt (OTP / verification code).
    Unanswerable(String),
}

/// Which answer a keyboard-interactive prompt gets: Some(password), Some(user) for a visible
/// username prompt, None when the stored password must not be used.
fn kbd_answer(prompt: &str, echo: bool, user: &str, password: &str) -> Option<String> {
    let p = prompt.trim();
    if echo {
        return if Regex::new(r"(?i)user(name)?|login|用户").unwrap().is_match(p) {
            Some(user.to_string())
        } else {
            None
        };
    }
    if OTP_PROMPT.is_match(p) && !Regex::new(r"(?i)^\s*password\s*:?\s*$").unwrap().is_match(p) {
        return None;
    }
    // An empty prompt text is how many PAM stacks ask for the password.
    (p.is_empty() || PASSWORD_PROMPT.is_match(p)).then(|| password.to_string())
}

async fn try_keyboard_interactive(handle: &mut H, user: &str, password: &str) -> Result<Kbd> {
    let mut resp = handle.authenticate_keyboard_interactive_start(user.to_string(), None).await?;
    for _ in 0..5 {
        match resp {
            KeyboardInteractiveAuthResponse::Success => return Ok(Kbd::Success),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(Kbd::Failed),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let mut answers = vec![];
                for p in &prompts {
                    match kbd_answer(&p.prompt, p.echo, user, password) {
                        Some(a) => answers.push(a),
                        None => return Ok(Kbd::Unanswerable(p.prompt.trim().to_string())),
                    }
                }
                resp = handle.authenticate_keyboard_interactive_respond(answers).await?;
            }
        }
    }
    Ok(Kbd::Failed)
}

/// Returns Some(true) on success, Some(false) when an agent was available but
/// no identity worked, None when no agent is reachable.
async fn try_agent(handle: &mut H, user: &str, a: &mut Attempts) -> Option<bool> {
    use russh::keys::agent::client::AgentClient;
    #[cfg(unix)]
    {
        if let Ok(agent) = AgentClient::connect_env().await {
            return Some(agent_identities(handle, user, agent, a).await);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
            return Some(agent_identities(handle, user, agent, a).await);
        }
        if let Ok(agent) = AgentClient::connect_pageant().await {
            return Some(agent_identities(handle, user, agent, a).await);
        }
    }
    None
}

async fn agent_identities<S>(handle: &mut H, user: &str, mut agent: russh::keys::agent::client::AgentClient<S>, a: &mut Attempts) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let Ok(ids) = agent.request_identities().await else { return false };
    if ids.len() > AGENT_MAX {
        a.notes.push(format!(
            "ssh-agent holds {} keys, only the first {AGENT_MAX} were offered (servers disconnect after too many); configure the right one with `--key`",
            ids.len()
        ));
    }
    let hash = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
    for id in ids.iter().take(AGENT_MAX) {
        let pk = id.public_key().into_owned();
        a.tried.push("publickey(agent)".into());
        match handle.authenticate_publickey_with(user.to_string(), pk, hash, &mut agent).await {
            Ok(r) if r.success() => return true,
            Ok(r) => {
                a.remaining = remaining_of(&r);
                if !a.allowed(MethodKind::PublicKey) {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_interactive_answers_only_password_prompts() {
        let pw = "s3cret";
        assert_eq!(kbd_answer("Password: ", false, "bob", pw).as_deref(), Some(pw));
        assert_eq!(kbd_answer("", false, "bob", pw).as_deref(), Some(pw));
        assert_eq!(kbd_answer("bob@host's password:", false, "bob", pw).as_deref(), Some(pw));
        assert_eq!(kbd_answer("密码：", false, "bob", pw).as_deref(), Some(pw));
        assert_eq!(kbd_answer("Verification code: ", false, "bob", pw), None);
        assert_eq!(kbd_answer("One-time password (OATH) for `bob':", false, "bob", pw), None);
        assert_eq!(kbd_answer("Duo two-factor login", false, "bob", pw), None);
        assert_eq!(kbd_answer("Enter PIN for token:", false, "bob", pw), None);
        assert_eq!(kbd_answer("Username:", true, "bob", pw).as_deref(), Some("bob"));
        assert_eq!(kbd_answer("Choose option (1/2):", true, "bob", pw), None);
    }
}
