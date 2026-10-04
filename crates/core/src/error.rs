//! Unified error type. Every error carries a stable machine-readable code and,
//! whenever possible, an actionable hint telling the agent what to do next.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    Usage,
    NotFound,
    AlreadyExists,
    Connect,
    Auth,
    HostKeyMismatch,
    Timeout,
    Secret,
    Remote,
    SessionClosed,
    Busy,
    Daemon,
    Io,
    Internal,
}

impl ErrorCode {
    /// Process exit code used by the CLI for this error class.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Usage | ErrorCode::AlreadyExists => 64,
            ErrorCode::NotFound => 66,
            ErrorCode::Connect => 69,
            ErrorCode::Auth | ErrorCode::Secret => 77,
            ErrorCode::HostKeyMismatch => 78,
            ErrorCode::Timeout => 124,
            ErrorCode::Busy => 75,
            _ => 125,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
            hint: None,
        }
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn usage(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Usage, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, m)
    }
    pub fn connect(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Connect, m)
    }
    pub fn auth(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Auth, m)
    }
    pub fn timeout(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Timeout, m)
    }
    pub fn remote(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Remote, m)
    }
    pub fn io(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Io, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, m)
    }
    pub fn secret(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Secret, m)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " (hint: {h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::io(e.to_string())
    }
}

#[cfg(feature = "ssh")]
impl From<russh::Error> for Error {
    fn from(e: russh::Error) -> Self {
        match e {
            russh::Error::ConnectionTimeout | russh::Error::KeepaliveTimeout | russh::Error::InactivityTimeout => {
                Error::timeout(format!("ssh: {e}"))
            }
            russh::Error::Disconnect => Error::connect("ssh: the server closed the connection during the SSH handshake")
                .hint("usually no common kex/cipher/host-key algorithm, or an IDS cutting the session; compare with `ssh -v`"),
            _ => Error::connect(format!("ssh: {e}")),
        }
    }
}

#[cfg(feature = "ssh")]
impl From<russh::keys::Error> for Error {
    fn from(e: russh::keys::Error) -> Self {
        Error::auth(format!("key: {e}"))
    }
}

#[cfg(feature = "ssh")]
impl From<russh_sftp::client::error::Error> for Error {
    fn from(e: russh_sftp::client::error::Error) -> Self {
        Error::remote(format!("sftp: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::internal(format!("json: {e}"))
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Error::io(format!("toml parse: {e}"))
    }
}

impl From<toml::ser::Error> for Error {
    fn from(e: toml::ser::Error) -> Self {
        Error::internal(format!("toml serialize: {e}"))
    }
}

impl From<regex::Error> for Error {
    fn from(e: regex::Error) -> Self {
        Error::usage(format!("invalid regex: {e}"))
    }
}
