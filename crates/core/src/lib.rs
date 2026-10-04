//! Shared layer of xssh, used by the engine, the CLI and the desktop app: the error type,
//! the home directory layout, the daemon protocol and local IPC, and text helpers.
//! Nothing here talks SSH, so front ends that only drive the daemon stay light.

pub mod api;
pub mod error;
pub mod guide;
pub mod ipc;
pub mod paths;
pub mod text;
pub mod transcript;
#[cfg(windows)]
pub mod winsec;

pub use error::{Error, ErrorCode, Result};
