//! Key pairs generated and managed by xssh. Private keys are stored on disk in
//! encrypted OpenSSH format; the random passphrase lives in the secret store
//! (keyring entries are too small for large RSA keys).

use rand::RngExt;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use xssh_core::error::{Error, Result};
use xssh_core::paths::{Paths, write_private};
use xssh_store::secrets::{SecretStore, key_passphrase};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyInfo {
    pub name: String,
    pub algorithm: String,
    pub fingerprint: String,
    pub public_key: String,
    pub path: String,
}

pub fn private_path(paths: &Paths, name: &str) -> PathBuf {
    paths.keys_dir().join(name)
}

pub fn generate(paths: &Paths, secrets: &SecretStore, name: &str, algo: &str) -> Result<KeyInfo> {
    xssh_store::hosts::validate_alias(name)?;
    let path = private_path(paths, name);
    if path.exists() {
        return Err(Error::new(
            xssh_core::error::ErrorCode::AlreadyExists,
            format!("key '{name}' already exists"),
        ));
    }
    let algorithm = match algo {
        "ed25519" => Algorithm::Ed25519,
        "rsa" => Algorithm::Rsa { hash: None },
        "ecdsa" => Algorithm::Ecdsa {
            curve: russh::keys::EcdsaCurve::NistP256,
        },
        other => return Err(Error::usage(format!("unsupported key type '{other}' (ed25519|rsa|ecdsa)"))),
    };
    let mut rng = rand::rng();
    let mut key = PrivateKey::random(&mut rng, algorithm).map_err(|e| Error::internal(format!("keygen: {e}")))?;
    key.set_comment(format!("xssh-{name}"));
    let pass: String = (0..40)
        .map(|_| {
            const CS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
            CS[rng.random_range(0..CS.len())] as char
        })
        .collect();
    let enc = key
        .encrypt(&mut rng, pass.as_bytes())
        .map_err(|e| Error::internal(format!("encrypt key: {e}")))?;
    let pem = enc
        .to_openssh(LineEnding::LF)
        .map_err(|e| Error::internal(format!("encode key: {e}")))?;
    secrets.set(&key_passphrase(name), &pass)?;
    write_private(&path, pem.as_bytes())?;
    let pubkey = key.public_key().to_openssh().map_err(|e| Error::internal(e.to_string()))?;
    std::fs::write(path.with_extension("pub"), format!("{pubkey}\n"))?;
    info(paths, name)
}

pub fn info(paths: &Paths, name: &str) -> Result<KeyInfo> {
    let path = private_path(paths, name);
    let pubpath = path.with_extension("pub");
    let pubstr =
        std::fs::read_to_string(&pubpath).map_err(|_| Error::not_found(format!("key '{name}' not found")).hint("run `xssh key list`"))?;
    let pk = russh::keys::PublicKey::from_openssh(pubstr.trim()).map_err(|e| Error::io(format!("{}: {e}", pubpath.display())))?;
    Ok(KeyInfo {
        name: name.to_string(),
        algorithm: pk.algorithm().to_string(),
        fingerprint: pk.fingerprint(HashAlg::Sha256).to_string(),
        public_key: pubstr.trim().to_string(),
        path: path.display().to_string(),
    })
}

pub fn list(paths: &Paths) -> Result<Vec<KeyInfo>> {
    let mut out = vec![];
    for e in std::fs::read_dir(paths.keys_dir())? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "pub")
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            && let Ok(i) = info(paths, stem)
        {
            out.push(i);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn remove(paths: &Paths, secrets: &SecretStore, name: &str) -> Result<()> {
    let path = private_path(paths, name);
    if !path.exists() {
        return Err(Error::not_found(format!("key '{name}' not found")));
    }
    std::fs::remove_file(&path)?;
    let _ = std::fs::remove_file(path.with_extension("pub"));
    secrets.delete(&key_passphrase(name))?;
    Ok(())
}

pub fn load(paths: &Paths, secrets: &SecretStore, name: &str) -> Result<PrivateKey> {
    let path = private_path(paths, name);
    let pass = secrets.get(&key_passphrase(name))?;
    russh::keys::load_secret_key(&path, pass.as_ref().map(|p| p.as_str())).map_err(|e| Error::auth(format!("load key '{name}': {e}")))
}
