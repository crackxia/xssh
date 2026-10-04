//! SSH connection establishment: host key policy, authentication chain,
//! ProxyJump / ProxyCommand, algorithm selection, and the shared connection pool.

pub mod auth;
pub mod keys;
pub mod known_hosts;
pub mod pool;
pub mod proxy;

use crate::ssh::known_hosts::{KnownHosts, Verdict, fingerprint};
use russh::client;
use russh::keys::{Algorithm, PublicKeyOrCertificate};
use russh::{Preferred, cipher, compression, kex, mac};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::paths::Paths;
use xssh_store::config::Config;
use xssh_store::hosts::Host;
use xssh_store::secrets::SecretStore;

/// Everything needed to open connections. Shared by the daemon and by the
/// in-process (`--no-daemon`) engine.
pub struct Ctx {
    pub paths: Paths,
    pub config: Config,
    pub secrets: SecretStore,
    pub known_hosts: KnownHosts,
}

impl Ctx {
    pub fn new(paths: Paths) -> Result<Self> {
        let config = Config::load(&paths)?;
        let secrets = SecretStore::open(&paths)?;
        let known_hosts = KnownHosts::new(&paths, config.use_system_known_hosts);
        Ok(Ctx {
            paths,
            config,
            secrets,
            known_hosts,
        })
    }
}

#[derive(Debug, Clone)]
pub enum HostKeyEvent {
    /// First contact under tofu; `saved` is false when known_hosts could not be written.
    Learned {
        fingerprint: String,
        saved: bool,
    },
    Mismatch {
        fingerprint: String,
        file: String,
        line: usize,
    },
    UnknownStrict {
        fingerprint: String,
    },
    Revoked {
        fingerprint: String,
        file: String,
        line: usize,
    },
}

/// Most connections kept for one host when the server limits channels per connection.
const MAX_CONNS_PER_HOST: usize = 8;

/// Remote port-forward targets: (bind address, bind port) -> (local host, local port).
pub type RemoteForwardMap = Arc<Mutex<HashMap<(String, u32), (String, u16)>>>;

/// Per remote forward (keyed like `RemoteForwardMap`): connections the server forwarded to us.
#[derive(Debug, Clone, Default)]
pub struct RemoteForwardStat {
    pub connections: u64,
    /// Forwarded connections whose local target could not be reached.
    pub failures: u64,
    pub last_error: Option<String>,
}
pub type RemoteForwardStats = Arc<Mutex<HashMap<(String, u32), RemoteForwardStat>>>;

pub struct ClientHandler {
    /// Name used for known_hosts: the host, or its HostKeyAlias.
    host: String,
    port: u16,
    strict: bool,
    known_hosts: KnownHosts,
    /// Only record the presented key and reject it (`host trust` shows the key first).
    probe: bool,
    presented: Arc<Mutex<Option<russh::keys::PublicKey>>>,
    hostkey: Arc<Mutex<Option<HostKeyEvent>>>,
    banner: Arc<Mutex<Option<String>>>,
    remote_forwards: RemoteForwardMap,
    remote_forward_stats: RemoteForwardStats,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn auth_banner(&mut self, banner: &str, _session: &mut client::Session) -> std::result::Result<(), Self::Error> {
        *self.banner.lock().unwrap() = Some(banner.to_string());
        Ok(())
    }

    async fn check_server_key(&mut self, server_public_key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        let fp = fingerprint(&key);
        *self.presented.lock().unwrap() = Some(key.clone());
        if self.probe {
            return Ok(false);
        }
        let kh = &self.known_hosts;
        let (ok, event) = match kh.verify(&self.host, self.port, &key) {
            Verdict::Known => (true, None),
            Verdict::Unknown if self.strict => (false, Some(HostKeyEvent::UnknownStrict { fingerprint: fp })),
            Verdict::Unknown => {
                let saved = kh.learn(&self.host, self.port, &key).is_ok();
                (true, Some(HostKeyEvent::Learned { fingerprint: fp, saved }))
            }
            Verdict::Changed { file, line } => (
                false,
                Some(HostKeyEvent::Mismatch {
                    fingerprint: fp,
                    file: file.display().to_string(),
                    line,
                }),
            ),
            Verdict::Revoked { file, line } => (
                false,
                Some(HostKeyEvent::Revoked {
                    fingerprint: fp,
                    file: file.display().to_string(),
                    line,
                }),
            ),
        };
        *self.hostkey.lock().unwrap() = event;
        Ok(ok)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> std::result::Result<(), Self::Error> {
        let target = {
            let map = self.remote_forwards.lock().unwrap();
            map.get_key_value(&(connected_address.to_string(), connected_port))
                .or_else(|| map.iter().find(|((_, p), _)| *p == connected_port))
                .map(|(k, v)| (k.clone(), v.clone()))
        };
        let Some((key, (lhost, lport))) = target else {
            drop(reply);
            return Ok(());
        };
        reply.accept().await;
        self.remote_forward_stats
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .connections += 1;
        let stats = self.remote_forward_stats.clone();
        tokio::spawn(async move {
            match tokio::net::TcpStream::connect((lhost.as_str(), lport)).await {
                Ok(mut tcp) => {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                }
                Err(e) => {
                    {
                        let mut st = stats.lock().unwrap();
                        let s = st.entry(key).or_default();
                        s.failures += 1;
                        s.last_error = Some(format!("connect {lhost}:{lport}: {e}"));
                    }
                    // Like OpenSSH: close the channel so the remote client sees the failure at
                    // once instead of waiting on a silent connection.
                    let _ = channel.close().await;
                }
            }
        });
        Ok(())
    }
}

/// An authenticated SSH connection.
pub struct Conn {
    pub alias: String,
    pub handle: client::Handle<ClientHandler>,
    pub remote_forwards: RemoteForwardMap,
    /// Counters for remote (-R) forwards, keyed like `remote_forwards`.
    pub remote_forward_stats: RemoteForwardStats,
    pub banner: Option<String>,
    pub hostkey_event: Option<HostKeyEvent>,
    pub auth_method: String,
    pub connected_at: Instant,
    pub connect_ms: u64,
    last_used: Mutex<Instant>,
    /// Keeps the jump connection alive for as long as this one lives.
    _jump: Option<Arc<Conn>>,
    /// Opens another connection to the same host (set by the pool).
    pub(crate) spawn: Option<Spawn>,
    /// Extra connection used once the server refuses more channels on this one
    /// (sshd MaxSessions, default 10); chained up to MAX_CONNS_PER_HOST connections.
    overflow: tokio::sync::Mutex<Option<Arc<Conn>>>,
    /// Position in the overflow chain (0 = the pooled connection).
    depth: usize,
    hostkey_reported: AtomicBool,
}

pub(crate) type Spawn = Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Conn>> + Send>> + Send + Sync>;

impl Conn {
    pub fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }
    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }
    /// The jump connection this one runs through.
    pub fn jump(&self) -> Option<&Arc<Conn>> {
        self._jump.as_ref()
    }
    pub fn open_session(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<russh::Channel<client::Msg>>> + Send + '_>> {
        Box::pin(async move {
            self.touch();
            let err = match self.handle.channel_open_session().await {
                Ok(ch) => return Ok(ch),
                Err(e) => e,
            };
            let err = Error::connect(format!("open channel on '{}': {err}", self.alias));
            let Some(spawn) = self.spawn.clone() else {
                return Err(err);
            };
            // A live connection refusing a channel has hit the server's per-connection
            // channel limit; a closed one died unnoticed while pooled (server restart, NAT
            // timeout, network change). Either way nothing was sent: continue on an overflow
            // connection (the pool replaces a dead connection on its next use).
            if self.is_closed() && self.depth + 1 >= MAX_CONNS_PER_HOST {
                return Err(err);
            }
            if self.depth + 1 >= MAX_CONNS_PER_HOST {
                return Err(Error::new(
                    ErrorCode::Busy,
                    format!(
                        "'{}' refuses more channels: {MAX_CONNS_PER_HOST} connections are open and each is full (sshd MaxSessions)",
                        self.alias
                    ),
                )
                .hint("close idle sessions (`xssh session list`, `session close`) or let running jobs finish, then retry"));
            }
            let next = {
                let mut o = self.overflow.lock().await;
                match o.as_ref().filter(|c| !c.is_closed()) {
                    Some(c) => c.clone(),
                    None => {
                        let mut c = spawn().await?;
                        c.spawn = Some(spawn.clone());
                        c.depth = self.depth + 1;
                        let c = Arc::new(c);
                        *o = Some(c.clone());
                        c
                    }
                }
            };
            next.open_session().await
        })
    }

    /// One line about a host key learned while connecting; returned once per connection.
    pub fn take_hostkey_note(&self) -> Option<String> {
        let note = match &self.hostkey_event {
            Some(HostKeyEvent::Learned { fingerprint, saved: true }) => {
                format!("new host key for {} trusted on first use: {fingerprint}", self.alias)
            }
            Some(HostKeyEvent::Learned { fingerprint, saved: false }) => format!(
                "new host key for {} accepted for this connection but NOT saved (known_hosts not writable): {fingerprint}",
                self.alias
            ),
            _ => return None,
        };
        (!self.hostkey_reported.swap(true, Ordering::Relaxed)).then_some(note)
    }

    /// Number of connections in this chain (1 + overflow connections).
    pub async fn chain_len(&self) -> usize {
        let mut n = 1;
        let mut next = self.overflow.lock().await.clone();
        while let Some(c) = next {
            n += 1;
            next = c.overflow.lock().await.clone();
        }
        n
    }

    /// Drop closed overflow connections from the chain.
    pub async fn prune_overflow(&self) {
        let mut o = self.overflow.lock().await;
        if o.as_ref().is_some_and(|c| c.is_closed()) {
            *o = None;
        }
    }
}

fn client_config(cfg: &Config, preferred: Preferred) -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: (cfg.keepalive_secs > 0).then(|| Duration::from_secs(cfg.keepalive_secs)),
        keepalive_max: 3,
        nodelay: true,
        preferred,
        ..Default::default()
    })
}

fn is_ext(k: &kex::Name) -> bool {
    [
        kex::EXTENSION_SUPPORT_AS_CLIENT,
        kex::EXTENSION_SUPPORT_AS_SERVER,
        kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
        kex::EXTENSION_OPENSSH_STRICT_KEX_AS_SERVER,
    ]
    .contains(k)
}

/// Algorithms offered to the server: OpenSSH's defaults (russh's plus ECDH key exchange and
/// aes128-gcm), host key types already recorded for this host first (so a server with several
/// host keys presents the known one), with `legacy` the SHA-1 / CBC algorithms old devices need,
/// and with `compress` zlib ahead of no compression.
pub fn preferred(recorded: &[Algorithm], legacy: bool, compress: bool) -> Preferred {
    let d = Preferred::DEFAULT;
    let mut kexes: Vec<kex::Name> = d.kex.iter().copied().filter(|k| !is_ext(k)).collect();
    kexes.extend([kex::ECDH_SHA2_NISTP256, kex::ECDH_SHA2_NISTP384, kex::ECDH_SHA2_NISTP521]);
    let mut ciphers: Vec<cipher::Name> = d.cipher.to_vec();
    ciphers.insert(2, cipher::AES_128_GCM);
    let mut macs: Vec<mac::Name> = d.mac.to_vec();
    let keys: Vec<Algorithm> = d.key.to_vec();
    if legacy {
        kexes.extend([kex::DH_G14_SHA1, kex::DH_GEX_SHA1, kex::DH_G1_SHA1]);
        ciphers.extend([
            cipher::AES_128_CBC,
            cipher::AES_192_CBC,
            cipher::AES_256_CBC,
            cipher::TRIPLE_DES_CBC,
        ]);
        macs.extend([mac::HMAC_SHA1_ETM, mac::HMAC_SHA1]);
    }
    kexes.extend(d.kex.iter().copied().filter(is_ext));
    // Known key types first, keeping their relative order from the default list.
    let rsa = |a: &Algorithm| matches!(a, Algorithm::Rsa { .. });
    let known = |a: &Algorithm| recorded.iter().any(|r| r == a || (rsa(r) && rsa(a)));
    let (mut first, rest): (Vec<Algorithm>, Vec<Algorithm>) = keys.into_iter().partition(known);
    first.extend(rest);
    let compression = if compress {
        vec![compression::ZLIB_LEGACY, compression::ZLIB, compression::NONE]
    } else {
        vec![compression::NONE, compression::ZLIB_LEGACY, compression::ZLIB]
    };
    Preferred {
        kex: Cow::Owned(kexes),
        key: Cow::Owned(first),
        cipher: Cow::Owned(ciphers),
        mac: Cow::Owned(macs),
        compression: Cow::Owned(compression),
        ..d
    }
}

/// Compression for a host: its own setting, else config.toml's.
pub fn compress_for(ctx: &Ctx, host: &Host) -> bool {
    host.compression.unwrap_or(ctx.config.compression)
}

/// Host key policy for a host: its own setting, else config.toml's.
pub fn strict_for(ctx: &Ctx, host: &Host) -> bool {
    host.host_key_policy.as_deref().unwrap_or(&ctx.config.host_key_policy) == "strict"
}

pub fn known_hosts_for(ctx: &Ctx, host: &Host) -> KnownHosts {
    KnownHosts::with_files(ctx.paths.known_hosts(), &host.known_hosts_files, ctx.config.use_system_known_hosts)
}

/// Name under which the host key is recorded (HostKeyAlias, else the address).
pub fn key_name(host: &Host) -> &str {
    host.host_key_alias.as_deref().unwrap_or(&host.host)
}

type BoxStream = Box<dyn Duplex>;
trait Duplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Duplex for T {}

const TUN_HINT: &str = "a local proxy/VPN (TUN) intercepts TCP on this machine: even 192.0.2.1 (a never-routed test address) accepts connections, so the proxy accepted this one and could not reach the target. Check the proxy's rule/outbound for this address and port (its connection log), or exclude the address from the TUN; not a server or credential problem";

/// Whether a local proxy/VPN answers TCP itself: a connection to TEST-NET-1 (192.0.2.1, never
/// routed on the internet) succeeds only when something on this machine accepts it.
async fn tcp_intercepted(port: u16) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(1500), tokio::net::TcpStream::connect(("192.0.2.1", port))).await,
        Ok(Ok(_))
    )
}

fn tcp_error(e: std::io::Error, host: &Host) -> Error {
    let hint = match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "nothing listens on that port: check the port and that sshd is running",
        std::io::ErrorKind::TimedOut => "host unreachable (firewall / wrong address / needs a jump host)",
        _ => "check the address, DNS and network path",
    };
    Error::connect(format!("tcp connect to {}:{}: {e}", host.host, host.port)).hint(hint)
}

/// A byte stream through the jump connection or a ProxyCommand (None: connect TCP directly).
async fn tunnel(host: &Host, jump: &Option<Arc<Conn>>) -> Result<Option<BoxStream>> {
    if let Some(j) = jump {
        let ch = j
            .handle
            .channel_open_direct_tcpip(host.host.clone(), host.port as u32, "127.0.0.1", 0)
            .await
            .map_err(|e| Error::connect(format!("jump via '{}' to {}:{} failed: {e}", j.alias, host.host, host.port)))?;
        return Ok(Some(Box::new(ch.into_stream())));
    }
    if let Some(pc) = &host.proxy_command {
        return Ok(Some(Box::new(proxy::spawn(pc, host)?)));
    }
    Ok(None)
}

/// The key the server presents, without authenticating or trusting it (`host trust`).
pub async fn probe_host_key(ctx: &Ctx, host: &Host, jump: Option<Arc<Conn>>) -> Result<russh::keys::PublicKey> {
    let presented = Arc::new(Mutex::new(None));
    let kh = known_hosts_for(ctx, host);
    let recorded = kh.recorded_algorithms(key_name(host), host.port);
    let handler = ClientHandler {
        host: key_name(host).to_string(),
        port: host.port,
        strict: true,
        known_hosts: kh,
        probe: true,
        presented: presented.clone(),
        hostkey: Arc::default(),
        banner: Arc::default(),
        remote_forwards: Arc::default(),
        remote_forward_stats: Arc::default(),
    };
    let config = client_config(&ctx.config, preferred(&recorded, host.legacy_algos, compress_for(ctx, host)));
    let timeout = Duration::from_secs(ctx.config.connect_timeout_secs.max(1));
    let fut = async {
        let r = match tunnel(host, &jump).await? {
            Some(s) => client::connect_stream(config, s, handler).await,
            None => {
                let tcp = tokio::net::TcpStream::connect((host.host.as_str(), host.port))
                    .await
                    .map_err(|e| tcp_error(e, host))?;
                client::connect_stream(config, tcp, handler).await
            }
        };
        r.map(|_| ()).map_err(Error::from)
    };
    let res = tokio::time::timeout(timeout, fut).await;
    let key = presented.lock().unwrap().clone();
    match (key, res) {
        (Some(k), _) => Ok(k),
        (None, Ok(Err(e))) => Err(e),
        (None, _) => Err(Error::connect(format!("{}:{} did not present a host key", host.host, host.port))),
    }
}

/// Open and authenticate a new connection to `host`, optionally tunnelled through `jump`.
pub async fn connect(ctx: &Ctx, host: &Host, jump: Option<Arc<Conn>>) -> Result<Conn> {
    let start = Instant::now();
    let hostkey = Arc::new(Mutex::new(None));
    let banner = Arc::new(Mutex::new(None));
    let remote_forwards: RemoteForwardMap = Arc::new(Mutex::new(HashMap::new()));
    let remote_forward_stats: RemoteForwardStats = Arc::default();
    let kh = known_hosts_for(ctx, host);
    let recorded = kh.recorded_algorithms(key_name(host), host.port);
    let handler = ClientHandler {
        host: key_name(host).to_string(),
        port: host.port,
        strict: strict_for(ctx, host),
        known_hosts: kh,
        probe: false,
        presented: Arc::default(),
        hostkey: hostkey.clone(),
        banner: banner.clone(),
        remote_forwards: remote_forwards.clone(),
        remote_forward_stats: remote_forward_stats.clone(),
    };
    let config = client_config(&ctx.config, preferred(&recorded, host.legacy_algos, compress_for(ctx, host)));
    let timeout = Duration::from_secs(ctx.config.connect_timeout_secs.max(1));
    let target = format!("{}@{}:{}", host.user, host.host, host.port);
    // First bytes received from the server; None until the TCP connection is up (direct connections only).
    let sniffed: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();

    let fut = async {
        if let Some(stream) = tunnel(host, &jump).await? {
            return client::connect_stream(config, stream, handler).await.map_err(|e| {
                let err = Error::from(e);
                match &host.proxy_command {
                    Some(pc) if jump.is_none() => {
                        let stderr = proxy::last_stderr();
                        err.hint(format!(
                            "check the ProxyCommand `{pc}`{}",
                            if stderr.is_empty() {
                                String::new()
                            } else {
                                format!("; it printed: {stderr}")
                            }
                        ))
                    }
                    _ => err,
                }
            });
        }
        // Connect TCP ourselves so network errors are reported precisely.
        let tcp = tokio::net::TcpStream::connect((host.host.as_str(), host.port))
            .await
            .map_err(|e| tcp_error(e, host))?;
        let _ = tcp.set_nodelay(true);
        // Record the first bytes the server sends so a failed handshake can be classified:
        // nothing at all = rejected before SSH started; non-"SSH-" data = not an SSH port.
        *sniffed.lock().unwrap() = Some(vec![]);
        let tcp = Sniff {
            inner: tcp,
            first: sniffed.clone(),
        };
        client::connect_stream(config, tcp, handler).await.map_err(|e| {
            let first = sniffed.lock().unwrap().clone().unwrap_or_default();
            if first.is_empty() {
                Error::connect(format!("TCP accepted, then closed/reset before any SSH banner ({e})"))
                .hint(
                    "nothing SSH answered: a firewall/IP block on the server side (security group, hosts.deny, fail2ban) or sshd MaxStartups exhausted. Not a credential problem; ask the user to check the firewall",
                )
            } else if !first.starts_with(b"SSH-") && !first.windows(4).any(|w| w == b"SSH-") {
                Error::connect(format!("not an SSH server; it sent {:?}", String::from_utf8_lossy(&first)))
                .hint("fix the port: `xssh host edit <alias> --port N`")
            } else {
                Error::from(e)
            }
        })
    };
    let res = tokio::time::timeout(timeout, fut).await;
    // TCP was accepted but no byte came back: find out whether a local proxy/TUN took it.
    let silent = matches!(sniffed.lock().unwrap().as_deref(), Some([]));
    let intercepted = silent && !matches!(res, Ok(Ok(_))) && tcp_intercepted(host.port).await;
    let handle = match res {
        Ok(Ok(h)) => h,
        Ok(Err(e)) if intercepted => return Err(e.hint(TUN_HINT)),
        Ok(Err(e)) => return Err(map_connect_error(e, host, &hostkey)),
        Err(_) => {
            let secs = timeout.as_secs();
            let (msg, hint) = match sniffed.lock().unwrap().as_deref() {
                None if jump.is_none() && host.proxy_command.is_none() => (
                    format!("tcp connect to {target} timed out after {secs}s"),
                    "host unreachable: wrong address/port, firewall, or needs a jump host (`xssh host edit <alias> --jump J`)".to_string(),
                ),
                None if host.proxy_command.is_some() => (
                    format!("no SSH handshake through the ProxyCommand to {target} within {secs}s"),
                    format!("check the ProxyCommand; it printed: {}", proxy::last_stderr()),
                ),
                Some([]) => (
                    format!("connected to {target} but no SSH banner within {secs}s"),
                    if intercepted {
                        TUN_HINT.to_string()
                    } else {
                        "port is probably not SSH (e.g. HTTP) or sshd is stalled".to_string()
                    },
                ),
                Some(b) if !b.windows(4).any(|w| w == b"SSH-") => (
                    format!("{target} is not an SSH server; it sent {:?}", String::from_utf8_lossy(b)),
                    "fix the port: `xssh host edit <alias> --port N`".to_string(),
                ),
                _ => (
                    format!("SSH handshake with {target} timed out after {secs}s"),
                    "slow or lossy network path; retry, or raise connect_timeout_secs in config.toml".to_string(),
                ),
            };
            return Err(Error::new(ErrorCode::Timeout, msg).hint(hint));
        }
    };
    let mut handle = handle;
    let auth_method = tokio::time::timeout(timeout * 2, auth::authenticate(ctx, host, &mut handle))
        .await
        .map_err(|_| Error::timeout(format!("authentication to {target} timed out")))??;
    let hostkey_event = hostkey.lock().unwrap().clone();
    let banner = banner.lock().unwrap().clone();
    Ok(Conn {
        alias: host.alias.clone(),
        handle,
        remote_forwards,
        remote_forward_stats,
        banner,
        hostkey_event,
        auth_method,
        connected_at: Instant::now(),
        connect_ms: start.elapsed().as_millis() as u64,
        last_used: Mutex::new(Instant::now()),
        _jump: jump,
        spawn: None,
        overflow: tokio::sync::Mutex::new(None),
        depth: 0,
        hostkey_reported: AtomicBool::new(false),
    })
}

fn map_connect_error(e: Error, host: &Host, hostkey: &Arc<Mutex<Option<HostKeyEvent>>>) -> Error {
    let trust = format!(
        "never bypass; ask the user to verify the presented fingerprint with the server's owner, then `xssh host trust {} --fingerprint FP` (the user confirms in a dialog)",
        host.alias
    );
    match hostkey.lock().unwrap().clone() {
        Some(HostKeyEvent::Mismatch { fingerprint, file, line }) => Error::new(
            ErrorCode::HostKeyMismatch,
            format!(
                "HOST KEY CHANGED for {}:{} — presented key {fingerprint} does not match {file}:{line}. \
                 This may indicate a man-in-the-middle attack or a reinstalled server.",
                host.host, host.port
            ),
        )
        .hint(trust),
        Some(HostKeyEvent::UnknownStrict { fingerprint }) => Error::new(
            ErrorCode::HostKeyMismatch,
            format!(
                "unknown host key {fingerprint} for {}:{} (host_key_policy=strict)",
                host.host, host.port
            ),
        )
        .hint(trust),
        Some(HostKeyEvent::Revoked { fingerprint, file, line }) => Error::new(
            ErrorCode::HostKeyMismatch,
            format!("host key {fingerprint} of {}:{} is REVOKED ({file}:{line})", host.host, host.port),
        )
        .hint("the key was explicitly revoked; do not connect, tell the user"),
        _ => {
            let mut e = e;
            let m = e.message.to_ascii_lowercase();
            if m.contains("no common") || m.contains("nocommonalgo") || m.contains("unknown algorithm") || m.contains("unknownalgo") {
                e = e.hint(format!(
                    "the server offers only algorithms that are off by default (old device?); allow them with `xssh host edit {} --legacy-algos`",
                    host.alias
                ));
            }
            if e.hint.is_none() {
                e = e.hint("check host/port with `xssh host show <alias>`; the host may be down or unreachable");
            }
            e.message = format!("connect to {}@{}:{}: {}", host.user, host.host, host.port, e.message);
            e
        }
    }
}

/// Stream wrapper that keeps a copy of the first bytes read from the server.
struct Sniff<S> {
    inner: S,
    first: Arc<Mutex<Option<Vec<u8>>>>,
}

const SNIFF_KEEP: usize = 64;

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Sniff<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Some(first) = self.first.lock().unwrap().as_mut()
            && first.len() < SNIFF_KEEP
        {
            let new = &buf.filled()[before..];
            first.extend_from_slice(&new[..new.len().min(SNIFF_KEEP - first.len())]);
        }
        r
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Sniff<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_orders_known_key_types_first_and_adds_legacy() {
        let p = preferred(&[Algorithm::Rsa { hash: None }], false, false);
        assert_eq!(p.compression[0], compression::NONE);
        assert_eq!(preferred(&[], false, true).compression[0], compression::ZLIB_LEGACY);
        assert!(matches!(p.key[0], Algorithm::Rsa { .. }));
        assert!(p.kex.contains(&kex::ECDH_SHA2_NISTP256));
        assert!(!p.kex.contains(&kex::DH_G14_SHA1));
        assert!(!p.cipher.contains(&cipher::AES_128_CBC));
        // Extension markers stay last.
        assert!(is_ext(p.kex.last().unwrap()));
        let l = preferred(&[], true, false);
        assert!(l.kex.contains(&kex::DH_G14_SHA1) && l.cipher.contains(&cipher::AES_128_CBC) && l.mac.contains(&mac::HMAC_SHA1));
        assert_eq!(l.key[0], Algorithm::Ed25519);
    }
}
