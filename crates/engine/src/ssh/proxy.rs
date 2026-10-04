//! ProxyCommand transport: the SSH connection runs over a local command's stdin/stdout
//! (e.g. `ssh -W %h:%p gw`, `cloudflared access ssh --hostname %h`, `nc -X 5 -x proxy:1080 %h %p`).

use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};
use xssh_core::error::{Error, Result};
use xssh_store::hosts::Host;
use xssh_store::ssh_config::expand_tokens;

/// Stderr of the most recent ProxyCommand (last 2 KB), for error messages.
static LAST_STDERR: Mutex<String> = Mutex::new(String::new());

pub fn last_stderr() -> String {
    LAST_STDERR.lock().unwrap().trim().to_string()
}

pub struct ProxyStream {
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// Killed when the connection is dropped.
    _child: Child,
}

/// Split a Windows command line into program + arguments (double quotes group words).
#[cfg_attr(not(windows), allow(dead_code))]
fn split_words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn spawn(template: &str, host: &Host) -> Result<ProxyStream> {
    let cmdline = expand_tokens(template, &host.host, host.port, &host.user, &host.alias);
    // Like OpenSSH: `sh -c 'exec CMD'` on Unix; the Windows port starts the program directly.
    #[cfg(unix)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(format!("exec {cmdline}"));
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let w = split_words(&cmdline);
        let Some((prog, args)) = w.split_first() else {
            return Err(Error::usage("empty ProxyCommand"));
        };
        let mut c = tokio::process::Command::new(prog);
        c.args(args);
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    };
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        Error::connect(format!("cannot start ProxyCommand `{cmdline}`: {e}")).hint("fix the ProxyCommand (program on PATH?)")
    })?;
    let stdin = child.stdin.take().ok_or_else(|| Error::internal("ProxyCommand stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| Error::internal("ProxyCommand stdout"))?;
    if let Some(mut err) = child.stderr.take() {
        LAST_STDERR.lock().unwrap().clear();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut s = LAST_STDERR.lock().unwrap();
                s.push_str(&String::from_utf8_lossy(&buf[..n]));
                if s.len() > 2048 {
                    let cut = s.len() - 2048;
                    let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
                    s.drain(..cut);
                }
            }
        });
    }
    Ok(ProxyStream {
        stdin,
        stdout,
        _child: child,
    })
}

impl AsyncRead for ProxyStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProxyStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(
            split_words(r#""C:\Program Files\x\nc.exe" -X 5 %h"#),
            vec![r"C:\Program Files\x\nc.exe", "-X", "5", "%h"]
        );
    }

    #[tokio::test]
    async fn proxy_stream_round_trip() {
        use tokio::io::AsyncWriteExt;
        // `cat`-like echo program available on both platforms.
        #[cfg(unix)]
        let tpl = "cat";
        #[cfg(windows)]
        let tpl = "powershell -NoProfile -Command \"$i=[Console]::OpenStandardInput();$o=[Console]::OpenStandardOutput();$i.CopyTo($o)\"";
        let host = Host {
            alias: "t".into(),
            host: "h".into(),
            port: 22,
            ..Default::default()
        };
        let mut s = spawn(tpl, &host).unwrap();
        s.write_all(b"SSH-2.0-test\n").await.unwrap();
        s.flush().await.unwrap();
        let mut buf = vec![0u8; 13];
        tokio::time::timeout(std::time::Duration::from_secs(20), s.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"SSH-2.0-test\n");
    }
}
