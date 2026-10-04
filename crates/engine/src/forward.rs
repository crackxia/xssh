//! Port forwarding managed by the daemon: `-L` (local -> remote), `-R` (remote -> local) and
//! `-D` (SOCKS5 proxy on this machine, connections leave from the host).
//!
//! Each accepted connection gets a fresh channel from the pool, so `-L`/`-D` survive a dropped
//! SSH connection. A `-R` listener lives on one connection: a supervisor re-requests it on a new
//! connection when that one drops. Failures are counted and the last error is kept for
//! `forward list`.

use crate::ssh::Conn;
use crate::ssh::pool::Pool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use xssh_core::error::{Error, Result};
use xssh_core::text::short_id;

pub use xssh_core::api::{ForwardInfo, Kind, Spec};

/// Counters shared with a forward's tasks.
#[derive(Default)]
struct Stats {
    connections: AtomicU64,
    failures: AtomicU64,
    reconnects: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// For `-R`: whether the listener is currently registered on a live connection.
    up: std::sync::atomic::AtomicBool,
}

impl Stats {
    fn fail(&self, e: impl std::fmt::Display) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(e.to_string());
    }
}

struct Forward {
    info: ForwardInfo,
    stats: Arc<Stats>,
    task: tokio::task::JoinHandle<()>,
    /// `-R`: the connection currently holding the remote listener.
    conn: Arc<Mutex<Option<Arc<Conn>>>>,
}

#[derive(Default)]
pub struct ForwardManager {
    items: Mutex<HashMap<String, Forward>>,
}

/// Open a direct-tcpip channel to `host:port` through `alias`'s pooled connection.
async fn open_direct(
    pool: &Pool,
    alias: &str,
    host: &str,
    port: u16,
    peer: std::net::SocketAddr,
) -> Result<russh::Channel<russh::client::Msg>> {
    let conn = pool.get(alias).await?;
    conn.handle
        .channel_open_direct_tcpip(host.to_string(), port as u32, peer.ip().to_string(), peer.port() as u32)
        .await
        .map_err(|e| Error::remote(format!("{alias} could not connect to {host}:{port}: {e}")))
}

impl ForwardManager {
    pub async fn add(&self, pool: Arc<Pool>, host: &str, mut spec: Spec) -> Result<ForwardInfo> {
        let id = format!("f{}", short_id(5));
        let stats = Arc::new(Stats::default());
        let conn_slot: Arc<Mutex<Option<Arc<Conn>>>> = Arc::default();
        let task = match spec.kind {
            Kind::Local | Kind::Dynamic => {
                let listener = tokio::net::TcpListener::bind((spec.bind_addr.as_str(), spec.bind_port))
                    .await
                    .map_err(|e| Error::io(format!("bind {}:{}: {e}", spec.bind_addr, spec.bind_port)))?;
                spec.bind_port = listener.local_addr()?.port();
                // Verify the host is reachable before reporting success.
                pool.get(host).await?;
                let host = host.to_string();
                let spec2 = spec.clone();
                let stats = stats.clone();
                tokio::spawn(async move {
                    while let Ok((tcp, peer)) = listener.accept().await {
                        stats.connections.fetch_add(1, Ordering::Relaxed);
                        let (pool, host, stats, spec) = (pool.clone(), host.clone(), stats.clone(), spec2.clone());
                        tokio::spawn(async move {
                            let r = if spec.kind == Kind::Dynamic {
                                socks5(tcp, peer, &pool, &host).await
                            } else {
                                relay(tcp, peer, &pool, &host, &spec.target_host, spec.target_port).await
                            };
                            if let Err(e) = r {
                                stats.fail(e.message);
                            }
                        });
                    }
                })
            }
            Kind::Remote => {
                let conn = pool.get(host).await?;
                let got = request_remote(&conn, &spec).await?;
                if spec.bind_port == 0 {
                    spec.bind_port = got;
                }
                *conn_slot.lock().unwrap() = Some(conn);
                stats.up.store(true, Ordering::Relaxed);
                tokio::spawn(supervise_remote(
                    pool,
                    host.to_string(),
                    spec.clone(),
                    stats.clone(),
                    conn_slot.clone(),
                ))
            }
        };
        let description = match spec.kind {
            Kind::Local => format!(
                "local {}:{} -> ({host}) {}:{}",
                spec.bind_addr, spec.bind_port, spec.target_host, spec.target_port
            ),
            Kind::Remote => format!(
                "({host}) {}:{} -> local {}:{}",
                spec.bind_addr, spec.bind_port, spec.target_host, spec.target_port
            ),
            Kind::Dynamic => format!("socks5 {}:{} -> out via ({host})", spec.bind_addr, spec.bind_port),
        };
        let info = ForwardInfo {
            id: id.clone(),
            host: host.to_string(),
            spec,
            created_at: xssh_store::audit::now_rfc3339(),
            connections: 0,
            alive: true,
            description,
            failures: 0,
            last_error: None,
            reconnects: 0,
        };
        self.items.lock().unwrap().insert(
            id,
            Forward {
                info: info.clone(),
                stats,
                task,
                conn: conn_slot,
            },
        );
        Ok(info)
    }

    pub fn list(&self) -> Vec<ForwardInfo> {
        let m = self.items.lock().unwrap();
        let mut v: Vec<ForwardInfo> = m
            .values()
            .map(|f| {
                let mut i = f.info.clone();
                if i.spec.kind == Kind::Remote
                    && let Some(c) = f.conn.lock().unwrap().as_ref()
                {
                    drain_remote(c, &i.spec, &f.stats);
                }
                i.connections = f.stats.connections.load(Ordering::Relaxed);
                i.failures = f.stats.failures.load(Ordering::Relaxed);
                i.reconnects = f.stats.reconnects.load(Ordering::Relaxed);
                i.last_error = f.stats.last_error.lock().unwrap().clone();
                i.alive = !f.task.is_finished()
                    && match i.spec.kind {
                        Kind::Remote => {
                            f.stats.up.load(Ordering::Relaxed) && f.conn.lock().unwrap().as_ref().is_some_and(|c| !c.is_closed())
                        }
                        _ => true,
                    };
                i
            })
            .collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        v
    }

    pub fn count(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    pub async fn stop(&self, id: &str) -> Result<ForwardInfo> {
        let f = self
            .items
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| Error::not_found(format!("no forward '{id}'")).hint("see `xssh forward list`"))?;
        f.task.abort();
        let conn = f.conn.lock().unwrap().take();
        if let Some(c) = conn {
            let s = &f.info.spec;
            c.remote_forwards.lock().unwrap().remove(&(s.bind_addr.clone(), s.bind_port as u32));
            let _ = c.handle.cancel_tcpip_forward(s.bind_addr.clone(), s.bind_port as u32).await;
        }
        Ok(f.info)
    }
}

/// `-L`: pipe one accepted connection to target through the host.
async fn relay(mut tcp: TcpStream, peer: std::net::SocketAddr, pool: &Pool, alias: &str, host: &str, port: u16) -> Result<()> {
    let ch = open_direct(pool, alias, host, port, peer).await?;
    let mut stream = ch.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
    Ok(())
}

/// Ask the server to listen for a `-R` forward on `conn` and route it; returns the bound port.
async fn request_remote(conn: &Arc<Conn>, spec: &Spec) -> Result<u16> {
    let got = conn
        .handle
        .tcpip_forward(spec.bind_addr.clone(), spec.bind_port as u32)
        .await
        .map_err(|e| {
            Error::remote(format!("server refused remote forward: {e}"))
                .hint("sshd may have AllowTcpForwarding disabled, or the port is in use")
        })?;
    let port = if spec.bind_port == 0 { got as u16 } else { spec.bind_port };
    conn.remote_forwards
        .lock()
        .unwrap()
        .insert((spec.bind_addr.clone(), port as u32), (spec.target_host.clone(), spec.target_port));
    Ok(port)
}

/// Move this forward's `-R` counters out of its connection (the ssh handler counts the channels
/// the server opens) into `stats`, so they survive reconnects.
fn drain_remote(conn: &Conn, spec: &Spec, stats: &Stats) {
    let key = (spec.bind_addr.clone(), spec.bind_port as u32);
    let Some(s) = conn.remote_forward_stats.lock().unwrap().remove(&key) else {
        return;
    };
    stats.connections.fetch_add(s.connections, Ordering::Relaxed);
    stats.failures.fetch_add(s.failures, Ordering::Relaxed);
    if s.last_error.is_some() {
        *stats.last_error.lock().unwrap() = s.last_error;
    }
}

/// Keep a `-R` forward registered: when its connection closes, request it again on a new one
/// (same port), retrying with backoff while the host is unreachable.
async fn supervise_remote(pool: Arc<Pool>, host: String, spec: Spec, stats: Arc<Stats>, slot: Arc<Mutex<Option<Arc<Conn>>>>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let alive = slot.lock().unwrap().as_ref().is_some_and(|c| !c.is_closed());
        if alive {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        if let Some(old) = slot.lock().unwrap().as_ref() {
            drain_remote(old, &spec, &stats);
        }
        stats.up.store(false, Ordering::Relaxed);
        let r = async {
            let conn = pool.get(&host).await?;
            request_remote(&conn, &spec).await?;
            Ok::<_, Error>(conn)
        }
        .await;
        match r {
            Ok(conn) => {
                *slot.lock().unwrap() = Some(conn);
                stats.up.store(true, Ordering::Relaxed);
                stats.reconnects.fetch_add(1, Ordering::Relaxed);
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                stats.fail(format!("re-establishing the remote listener: {}", e.message));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// A parsed SOCKS request: destination host and port.
#[derive(Debug, PartialEq)]
struct SocksTarget {
    host: String,
    port: u16,
}

/// Minimal SOCKS5 (RFC 1928, no authentication, CONNECT only) and SOCKS4/4a server side.
async fn socks5(mut tcp: TcpStream, peer: std::net::SocketAddr, pool: &Pool, alias: &str) -> Result<()> {
    let io = |e: std::io::Error| Error::io(format!("socks client {peer}: {e}"));
    let ver = tcp.read_u8().await.map_err(io)?;
    let (target, v4) = match ver {
        5 => {
            let n = tcp.read_u8().await.map_err(io)? as usize;
            let mut methods = vec![0u8; n];
            tcp.read_exact(&mut methods).await.map_err(io)?;
            if !methods.contains(&0) {
                let _ = tcp.write_all(&[5, 0xff]).await;
                return Err(Error::usage(format!("socks client {peer} offered no 'no authentication' method")));
            }
            tcp.write_all(&[5, 0]).await.map_err(io)?;
            let mut head = [0u8; 4];
            tcp.read_exact(&mut head).await.map_err(io)?;
            if head[1] != 1 {
                let _ = tcp.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                return Err(Error::usage(format!("socks client {peer}: only CONNECT is supported")));
            }
            let host = match head[3] {
                1 => {
                    let mut a = [0u8; 4];
                    tcp.read_exact(&mut a).await.map_err(io)?;
                    std::net::Ipv4Addr::from(a).to_string()
                }
                3 => {
                    let n = tcp.read_u8().await.map_err(io)? as usize;
                    let mut d = vec![0u8; n];
                    tcp.read_exact(&mut d).await.map_err(io)?;
                    String::from_utf8_lossy(&d).to_string()
                }
                4 => {
                    let mut a = [0u8; 16];
                    tcp.read_exact(&mut a).await.map_err(io)?;
                    std::net::Ipv6Addr::from(a).to_string()
                }
                t => return Err(Error::usage(format!("socks client {peer}: bad address type {t}"))),
            };
            let port = tcp.read_u16().await.map_err(io)?;
            (SocksTarget { host, port }, false)
        }
        4 => {
            let cmd = tcp.read_u8().await.map_err(io)?;
            let port = tcp.read_u16().await.map_err(io)?;
            let mut ip = [0u8; 4];
            tcp.read_exact(&mut ip).await.map_err(io)?;
            read_cstr(&mut tcp).await.map_err(io)?; // user id
            // SOCKS4a: 0.0.0.x means "the host name follows".
            let host = if ip[..3] == [0, 0, 0] && ip[3] != 0 {
                read_cstr(&mut tcp).await.map_err(io)?
            } else {
                std::net::Ipv4Addr::from(ip).to_string()
            };
            if cmd != 1 {
                let _ = tcp.write_all(&[0, 91, 0, 0, 0, 0, 0, 0]).await;
                return Err(Error::usage(format!("socks client {peer}: only CONNECT is supported")));
            }
            (SocksTarget { host, port }, true)
        }
        v => return Err(Error::usage(format!("socks client {peer}: unsupported SOCKS version {v}"))),
    };
    match open_direct(pool, alias, &target.host, target.port, peer).await {
        Ok(ch) => {
            let ok: &[u8] = if v4 {
                &[0, 90, 0, 0, 0, 0, 0, 0]
            } else {
                &[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]
            };
            tcp.write_all(ok).await.map_err(io)?;
            let mut stream = ch.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
            Ok(())
        }
        Err(e) => {
            let fail: &[u8] = if v4 {
                &[0, 91, 0, 0, 0, 0, 0, 0]
            } else {
                &[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]
            };
            let _ = tcp.write_all(fail).await;
            Err(e)
        }
    }
}

async fn read_cstr(tcp: &mut TcpStream) -> std::io::Result<String> {
    let mut v = vec![];
    loop {
        let b = tcp.read_u8().await?;
        if b == 0 || v.len() > 255 {
            break;
        }
        v.push(b);
    }
    Ok(String::from_utf8_lossy(&v).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specs() {
        let s = Spec::parse(Kind::Local, "8080:localhost:80").unwrap();
        assert_eq!(
            (s.bind_addr.as_str(), s.bind_port, s.target_host.as_str(), s.target_port),
            ("127.0.0.1", 8080, "localhost", 80)
        );
        let s = Spec::parse(Kind::Local, "0.0.0.0:5432:db.internal:5432").unwrap();
        assert_eq!(s.bind_addr, "0.0.0.0");
        let s = Spec::parse(Kind::Local, "[::1]:9000:[fe80::1]:22").unwrap();
        assert_eq!((s.bind_addr.as_str(), s.target_host.as_str()), ("::1", "fe80::1"));
        assert!(Spec::parse(Kind::Local, "abc").is_err());
        let d = Spec::parse(Kind::Dynamic, "1080").unwrap();
        assert_eq!((d.bind_addr.as_str(), d.bind_port), ("127.0.0.1", 1080));
        let d = Spec::parse(Kind::Dynamic, "0.0.0.0:1080").unwrap();
        assert_eq!(d.bind_addr, "0.0.0.0");
        assert!(Spec::parse(Kind::Dynamic, "1:2:3").is_err());
    }

    #[test]
    fn stats_keep_last_error() {
        let s = Stats::default();
        s.fail("a");
        s.fail("b");
        assert_eq!(s.failures.load(Ordering::Relaxed), 2);
        assert_eq!(s.last_error.lock().unwrap().as_deref(), Some("b"));
    }
}
