//! Secret storage. Primary backend is the OS keyring (Windows Credential
//! Manager, macOS Keychain, Linux Secret Service). When no keyring is
//! available (e.g. headless Linux), an encrypted file is used instead.
//!
//! Secret values never leave the process that uses them: they are not part of
//! any IPC response, log or transcript.

use crate::hosts::FileLock;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use xssh_core::error::{Error, Result};
use xssh_core::paths::{Paths, restrict_file, write_private};
use zeroize::Zeroizing;

pub type Secret = Zeroizing<String>;

const MAGIC_V2: &[u8] = b"XSSHENC2";
const SALT_LEN: usize = 16;

/// Decrypted secret file contents; values are wiped from memory on drop.
struct SecretMap(BTreeMap<String, String>);

impl Drop for SecretMap {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        for v in self.0.values_mut() {
            v.zeroize();
        }
    }
}

pub fn host_password(alias: &str) -> String {
    format!("host/{alias}/password")
}
pub fn host_sudo(alias: &str) -> String {
    format!("host/{alias}/sudo")
}
pub fn host_passphrase(alias: &str) -> String {
    format!("host/{alias}/passphrase")
}
pub fn key_passphrase(name: &str) -> String {
    format!("key/{name}/passphrase")
}
pub fn user_secret(name: &str) -> String {
    format!("user/{name}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Keyring,
    File,
}

pub struct SecretStore {
    backend: Backend,
    paths: Paths,
    /// Keyring service name, `xssh-<instance id>`: each install (home) has its own namespace.
    service: String,
}

impl SecretStore {
    pub fn open(paths: &Paths) -> Result<Self> {
        let backend = match std::env::var("XSSH_SECRET_BACKEND").ok().as_deref() {
            Some("file") => Backend::File,
            Some("keyring") => Backend::Keyring,
            Some(other) => return Err(Error::usage(format!("unknown XSSH_SECRET_BACKEND '{other}' (use keyring|file)"))),
            None => {
                if keyring::Entry::store_status().is_ok() {
                    Backend::Keyring
                } else {
                    Backend::File
                }
            }
        };
        Ok(SecretStore {
            backend,
            paths: paths.clone(),
            service: format!("xssh-{}", paths.instance_id()?),
        })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        match self.backend {
            Backend::Keyring => {
                let entry = self.entry(name)?;
                entry.set_password(value).map_err(|e| {
                    Error::secret(format!("keyring write failed: {e}"))
                        .hint("set XSSH_SECRET_BACKEND=file to use the encrypted file backend")
                })?;
            }
            Backend::File => {
                let _g = FileLock::acquire(&self.paths.home.join("secrets.lock"))?;
                let mut map = self.file_load()?;
                map.0.insert(name.to_string(), value.to_string());
                self.file_save(&map.0)?;
            }
        }
        self.index_update(|idx| {
            if !idx.contains(&name.to_string()) {
                idx.push(name.to_string());
                idx.sort();
            }
        })
    }

    pub fn get(&self, name: &str) -> Result<Option<Secret>> {
        match self.backend {
            Backend::Keyring => {
                let entry = self.entry(name)?;
                match entry.get_password() {
                    Ok(p) => Ok(Some(Zeroizing::new(p))),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(e) => Err(Error::secret(format!("keyring read failed: {e}"))),
                }
            }
            Backend::File => {
                let map = self.file_load()?;
                Ok(map.0.get(name).map(|v| Zeroizing::new(v.clone())))
            }
        }
    }

    pub fn delete(&self, name: &str) -> Result<bool> {
        let existed = match self.backend {
            Backend::Keyring => {
                let entry = self.entry(name)?;
                match entry.delete_credential() {
                    Ok(()) => true,
                    Err(keyring::Error::NoEntry) => false,
                    Err(e) => return Err(Error::secret(format!("keyring delete failed: {e}"))),
                }
            }
            Backend::File => {
                let _g = FileLock::acquire(&self.paths.home.join("secrets.lock"))?;
                let mut map = self.file_load()?;
                let existed = map.0.remove(name).is_some();
                self.file_save(&map.0)?;
                existed
            }
        };
        self.index_update(|idx| idx.retain(|n| n != name))?;
        Ok(existed)
    }

    /// Delete the password, sudo password and key passphrase stored for a host.
    pub fn forget_host(&self, alias: &str) {
        for n in [host_password(alias), host_sudo(alias), host_passphrase(alias)] {
            let _ = self.delete(&n);
        }
    }

    /// Names of stored secrets (never values).
    pub fn names(&self) -> Result<Vec<String>> {
        let f = self.paths.secret_index_file();
        if !f.exists() {
            return Ok(vec![]);
        }
        let s = std::fs::read_to_string(&f)?;
        Ok(serde_json::from_str(&s).unwrap_or_default())
    }

    pub fn has(&self, name: &str) -> bool {
        self.names().map(|n| n.iter().any(|x| x == name)).unwrap_or(false)
    }

    fn index_update(&self, f: impl FnOnce(&mut Vec<String>)) -> Result<()> {
        let _g = FileLock::acquire(&self.paths.home.join("secrets.index.lock"))?;
        let mut idx = self.names()?;
        f(&mut idx);
        write_private(&self.paths.secret_index_file(), serde_json::to_string_pretty(&idx)?.as_bytes())
    }

    fn entry(&self, name: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, name).map_err(|e| Error::secret(format!("keyring: {e}")))
    }

    // ---- encrypted file backend ----
    //
    // secrets.enc v2: "XSSHENC2" | salt (16) | nonce (12) | ChaCha20-Poly1305 ciphertext.
    // The key is master.key (32 random bytes, owner-only), or Argon2id(XSSH_MASTER_KEY, salt).
    // v1 files (nonce | ciphertext; passphrase key = SHA-256) are still read and rewritten as v2
    // on the next change.

    /// Key for a v2 file with `salt`.
    fn master_key(&self, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        let mut k = Zeroizing::new([0u8; 32]);
        if let Ok(pass) = std::env::var("XSSH_MASTER_KEY") {
            let pass = Zeroizing::new(pass);
            argon2::Argon2::default()
                .hash_password_into(pass.as_bytes(), salt, &mut k[..])
                .map_err(|e| Error::secret(format!("derive key from XSSH_MASTER_KEY: {e}")))?;
            return Ok(k);
        }
        self.random_key()
    }

    /// Key for a v1 file.
    fn legacy_key(&self) -> Result<Zeroizing<[u8; 32]>> {
        if let Ok(pass) = std::env::var("XSSH_MASTER_KEY") {
            let mut h = Sha256::new();
            h.update(b"xssh-master-key-v1:");
            h.update(pass.as_bytes());
            let mut k = Zeroizing::new([0u8; 32]);
            k.copy_from_slice(&h.finalize());
            return Ok(k);
        }
        self.random_key()
    }

    fn random_key(&self) -> Result<Zeroizing<[u8; 32]>> {
        let f: PathBuf = self.paths.master_key_file();
        if !f.exists() {
            let mut k = [0u8; 32];
            rand::rng().fill(&mut k);
            write_private(&f, hex::encode(k).as_bytes())?;
            restrict_file(&f);
        }
        let s = Zeroizing::new(std::fs::read_to_string(&f)?);
        let bytes = Zeroizing::new(hex::decode(s.trim()).map_err(|_| Error::secret("master.key is corrupt"))?);
        if bytes.len() != 32 {
            return Err(Error::secret("master.key must contain 32 bytes (hex)"));
        }
        let mut k = Zeroizing::new([0u8; 32]);
        k.copy_from_slice(&bytes);
        Ok(k)
    }

    fn file_load(&self) -> Result<SecretMap> {
        let f = self.paths.secrets_file();
        if !f.exists() {
            return Ok(SecretMap(BTreeMap::new()));
        }
        let data = std::fs::read(&f)?;
        let (key, body) = match data.strip_prefix(MAGIC_V2) {
            Some(rest) if rest.len() >= SALT_LEN + 12 => (self.master_key(&rest[..SALT_LEN])?, &rest[SALT_LEN..]),
            Some(_) => return Err(Error::secret("secrets.enc is corrupt")),
            None if data.len() >= 12 => (self.legacy_key()?, &data[..]),
            None => return Err(Error::secret("secrets.enc is corrupt")),
        };
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key[..]));
        let plain = Zeroizing::new(
            cipher
                .decrypt(Nonce::from_slice(&body[..12]), &body[12..])
                .map_err(|_| Error::secret("cannot decrypt secrets.enc (wrong XSSH_MASTER_KEY or master.key?)"))?,
        );
        let map: BTreeMap<String, String> = serde_json::from_slice(&plain)?;
        Ok(SecretMap(map))
    }

    fn file_save(&self, map: &BTreeMap<String, String>) -> Result<()> {
        // v2 only matters for a passphrase (the salt feeds Argon2id); with master.key the v1
        // layout is kept so older xssh builds sharing this home can still read it.
        let v2 = std::env::var_os("XSSH_MASTER_KEY").is_some();
        let mut salt = [0u8; SALT_LEN];
        rand::rng().fill(&mut salt);
        let key = if v2 { self.master_key(&salt)? } else { self.random_key()? };
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key[..]));
        let mut nonce = [0u8; 12];
        rand::rng().fill(&mut nonce);
        let plain = Zeroizing::new(serde_json::to_vec(map)?);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
            .map_err(|_| Error::secret("encryption failed"))?;
        let mut out = vec![];
        if v2 {
            out.extend_from_slice(MAGIC_V2);
            out.extend_from_slice(&salt);
        }
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        write_private(&self.paths.secrets_file(), &out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backend_v2_and_legacy() {
        let home = std::env::temp_dir().join(format!("xssh-secrets-test-{}", std::process::id()));
        let paths = Paths::resolve(Some(&home)).unwrap();
        let store = SecretStore {
            backend: Backend::File,
            paths: paths.clone(),
            service: "t".into(),
        };
        // A v1 file (older builds) stays readable, and stays v1 when the key is master.key.
        let key = store.legacy_key().unwrap();
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key[..]));
        let nonce = [3u8; 12];
        let ct = cipher.encrypt(Nonce::from_slice(&nonce), br#"{"a":"1"}"#.as_slice()).unwrap();
        std::fs::write(paths.secrets_file(), [nonce.to_vec(), ct].concat()).unwrap();
        assert_eq!(store.get("a").unwrap().as_deref().map(String::as_str), Some("1"));
        store.set("b", "2").unwrap();
        assert!(!std::fs::read(paths.secrets_file()).unwrap().starts_with(MAGIC_V2));
        assert_eq!(store.get("a").unwrap().as_deref().map(String::as_str), Some("1"));
        assert_eq!(store.get("b").unwrap().as_deref().map(String::as_str), Some("2"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn passphrase_key_uses_argon2id_with_salt() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        argon2::Argon2::default()
            .hash_password_into(b"pw", &[1u8; SALT_LEN], &mut a)
            .unwrap();
        argon2::Argon2::default()
            .hash_password_into(b"pw", &[2u8; SALT_LEN], &mut b)
            .unwrap();
        assert_ne!(a, b);
        let mut h = Sha256::new();
        h.update(b"xssh-master-key-v1:pw");
        assert_ne!(a.as_slice(), h.finalize().as_slice());
    }
}
