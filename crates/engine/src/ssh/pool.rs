//! Connection pool: one authenticated connection per host alias, reused across
//! requests; reconnects transparently when a connection dropped, the host
//! definition changed, or its jump connection was replaced.

use super::{Conn, Ctx, connect};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use xssh_core::error::{Error, Result};
use xssh_store::hosts::{Host, HostStore, jump_chain};

struct Slot {
    conn: Option<(Host, Arc<Conn>)>,
}

pub struct Pool {
    pub ctx: Arc<Ctx>,
    slots: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Slot>>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnInfo {
    pub alias: String,
    pub auth: String,
    pub age_secs: u64,
    pub idle_secs: u64,
    /// Connections in use for this host (>1 once sshd's per-connection channel limit was hit).
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub conns: usize,
}

fn one() -> usize {
    1
}
fn is_one(n: &usize) -> bool {
    *n == 1
}

/// Pool key: the alias, or the alias reached through a ProxyJump chain prefix.
fn slot_key(alias: &str, via: Option<&str>) -> String {
    match via {
        Some(v) => format!("{alias}\u{0}via {v}"),
        None => alias.to_string(),
    }
}

impl Pool {
    pub fn new(ctx: Arc<Ctx>) -> Self {
        Pool {
            ctx,
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub fn hosts(&self) -> HostStore {
        HostStore::new(&self.ctx.paths)
    }

    pub async fn get(&self, alias: &str) -> Result<Arc<Conn>> {
        self.get_via(alias.to_string(), None, vec![]).await
    }

    /// The jump connection for `host` (the last hop of its ProxyJump chain), if it has one.
    pub async fn jump_for(&self, host: &Host) -> Result<Option<Arc<Conn>>> {
        self.jump_conn(host, std::slice::from_ref(&host.alias)).await
    }

    async fn jump_conn(&self, host: &Host, path: &[String]) -> Result<Option<Arc<Conn>>> {
        let Some(spec) = host.jump.as_deref() else { return Ok(None) };
        let hops: Vec<&str> = jump_chain(spec).collect();
        let Some((last, prefix)) = hops.split_last() else { return Ok(None) };
        let prefix = (!prefix.is_empty()).then(|| prefix.join(","));
        let c = self.get_via(last.to_string(), prefix, path.to_vec()).await.map_err(|e| {
            let mut e = e;
            if !e.message.starts_with("jump host") {
                e.message = format!("jump host '{last}': {}", e.message);
            }
            e
        })?;
        Ok(Some(c))
    }

    /// `via` replaces the host's own jump setting (a hop inside a ProxyJump chain). `path`
    /// holds the hosts already being connected, so a cycle is an error instead of a deadlock.
    fn get_via(
        &self,
        alias: String,
        via: Option<String>,
        path: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<Arc<Conn>>> + Send + '_>> {
        Box::pin(async move {
            if path.contains(&alias) {
                let mut cycle = path.clone();
                cycle.push(alias.clone());
                return Err(Error::usage(format!("jump host cycle: {}", cycle.join(" -> "))).hint(format!(
                    "fix the jump settings: `xssh host edit {} --no-jump` or point it at another host",
                    path[0]
                )));
            }
            let mut host = self.hosts().get(&alias)?;
            if let Some(v) = &via {
                host.jump = Some(v.clone());
            }
            let mut next = path.clone();
            next.push(alias.clone());
            // Resolve the jump first (it has its own slot), so no slot lock is held while
            // connecting through a chain.
            let jump = self.jump_conn(&host, &next).await?;
            let slot = {
                let mut slots = self.slots.lock().unwrap();
                slots
                    .entry(slot_key(&alias, via.as_deref()))
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Slot { conn: None })))
                    .clone()
            };
            let mut slot = slot.lock().await;
            if let Some((h, c)) = &slot.conn
                && !c.is_closed()
                && *h == host_identity(&host)
                && same_jump(c.jump(), jump.as_ref())
            {
                c.touch();
                return Ok(c.clone());
            }
            slot.conn = None;
            let mut conn = connect(&self.ctx, &host, jump.clone()).await.map_err(|e| with_history(e, &host))?;
            if !host.ad_hoc {
                let _ = self.hosts().record_ok(&host.alias, &host.addr());
            }
            let (ctx, h) = (self.ctx.clone(), host.clone());
            conn.spawn = Some(Arc::new(move || {
                let (ctx, h, jump) = (ctx.clone(), h.clone(), jump.clone());
                Box::pin(async move { connect(&ctx, &h, jump).await })
            }));
            let conn = Arc::new(conn);
            slot.conn = Some((host_identity(&host), conn.clone()));
            Ok(conn)
        })
    }

    /// Drop the pooled connection for an alias, including its entries as a chain hop (it
    /// closes once no session uses it).
    pub async fn drop_conn(&self, alias: &str) {
        let prefix = format!("{alias}\u{0}");
        let slots: Vec<_> = self
            .slots
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| *k == alias || k.starts_with(&prefix))
            .map(|(_, v)| v.clone())
            .collect();
        for slot in slots {
            slot.lock().await.conn = None;
        }
    }

    pub async fn list(&self) -> Vec<ConnInfo> {
        let slots: Vec<_> = self.slots.lock().unwrap().values().cloned().collect();
        let mut out = vec![];
        for s in slots {
            if let Some((_, c)) = &s.lock().await.conn
                && !c.is_closed()
            {
                out.push(ConnInfo {
                    alias: c.alias.clone(),
                    auth: c.auth_method.clone(),
                    age_secs: c.connected_at.elapsed().as_secs(),
                    idle_secs: c.idle_for().as_secs(),
                    conns: c.chain_len().await,
                });
            }
        }
        out.sort_by(|a, b| a.alias.cmp(&b.alias));
        out
    }

    /// Drop connections that are closed, or idle and not used by anyone else; forget closed
    /// overflow connections.
    pub async fn gc(&self, idle: Duration) {
        let slots: Vec<_> = self.slots.lock().unwrap().values().cloned().collect();
        for s in slots {
            let Ok(mut slot) = s.try_lock() else { continue };
            let drop_it = match &slot.conn {
                Some((_, c)) => c.is_closed() || (c.idle_for() > idle && Arc::strong_count(c) == 1),
                None => false,
            };
            if drop_it {
                slot.conn = None;
            } else if let Some((_, c)) = &slot.conn {
                c.prune_overflow().await;
            }
        }
    }
}

fn same_jump(a: Option<&Arc<Conn>>, b: Option<&Arc<Conn>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// A connect failure of a saved host, with what is known about its last success: a stale
/// address (cloud IP changed, machine released) is the usual cause when it used to work.
fn with_history(e: Error, h: &Host) -> Error {
    use xssh_core::error::ErrorCode;
    // A local proxy/TUN failure says nothing about the host itself.
    let local = e.hint.as_deref().is_some_and(|x| x.contains("(TUN)"));
    if h.ad_hoc || local || !matches!(e.code, ErrorCode::Connect | ErrorCode::Timeout) {
        return e;
    }
    let addr = h.addr();
    let f = h.facts.clone().unwrap_or_default();
    let mut note = match (&f.last_ok, &f.last_addr) {
        (None, _) => "no successful connection recorded: check address/port".to_string(),
        (Some(t), Some(a)) if *a != addr => format!("last ok {} at {a} (address since changed: verify it)", short_time(t)),
        (Some(t), _) => format!(
            "last ok {} at this address: server down, address changed (cloud IP, released machine) or firewall changed; ask the user for the current address, then `xssh host edit {} --host NEW`",
            short_time(t),
            h.alias
        ),
    };
    if h.is_expired() {
        note = format!(
            "host expired {} (probably released; `xssh host rm {}`); {note}",
            h.expires.as_deref().map(short_time).unwrap_or_default(),
            h.alias
        );
    }
    let hint = match &e.hint {
        Some(old) => format!("{old}; {note}"),
        None => note,
    };
    e.hint(hint)
}

/// `2026-10-04T12:30:05.123+08:00` -> `2026-10-04 12:30`.
fn short_time(t: &str) -> String {
    t.get(..16).map(|s| s.replace('T', " ")).unwrap_or_else(|| t.to_string())
}

/// The parts of a host definition that require reconnecting when changed.
fn host_identity(h: &Host) -> Host {
    Host {
        tags: vec![],
        note: None,
        facts: None,
        encoding: None,
        expires: None,
        ..h.clone()
    }
}
