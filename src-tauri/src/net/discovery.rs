//! Descoberta serverless de peers via UDP broadcast na sub-rede do Radmin VPN.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use uuid::Uuid;

use super::protocol::{PeerStatus, MAGIC, PROTO_VERSION};

pub const BEACON_INTERVAL: Duration = Duration::from_secs(2);
pub const PEER_TTL: Duration = Duration::from_secs(6);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Beacon {
    pub magic: String,
    pub v: u8,
    pub id: Uuid,
    pub nick: String,
    pub status: PeerStatus,
    pub ctrl_port: u16,
    pub seq: u64,
    pub bye: bool,
}

/// Visão serializável de um peer para a UI.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct PeerInfo {
    pub id: Uuid,
    pub nick: String,
    pub status: PeerStatus,
    pub ip: Ipv4Addr,
    pub ctrl_port: u16,
}

/// Identidade local (mutável: apelido/estado).
pub struct LocalPeer {
    pub id: Uuid,
    pub ctrl_port: u16,
    nick: Mutex<String>,
    status: Mutex<PeerStatus>,
    seq: Mutex<u64>,
    changed: Notify,
}

impl LocalPeer {
    pub fn new(nick: String, ctrl_port: u16) -> Arc<Self> {
        Arc::new(Self {
            id: Uuid::new_v4(),
            ctrl_port,
            nick: Mutex::new(nick),
            status: Mutex::new(PeerStatus::Idle),
            seq: Mutex::new(0),
            changed: Notify::new(),
        })
    }

    pub fn set_status(&self, s: PeerStatus) {
        let mut cur = self.status.lock();
        if *cur != s {
            *cur = s;
            drop(cur);
            self.changed.notify_one(); // beacon imediato
        }
    }

    pub fn status(&self) -> PeerStatus {
        *self.status.lock()
    }

    pub fn set_nick(&self, n: String) {
        *self.nick.lock() = n;
        self.changed.notify_one();
    }

    pub fn nick(&self) -> String {
        self.nick.lock().clone()
    }

    pub fn beacon(&self, bye: bool) -> Beacon {
        let mut s = self.seq.lock();
        *s += 1;
        Beacon {
            magic: MAGIC.into(),
            v: PROTO_VERSION,
            id: self.id,
            nick: self.nick(),
            status: self.status(),
            ctrl_port: self.ctrl_port,
            seq: *s,
            bye,
        }
    }
}

struct Entry {
    info: PeerInfo,
    last_seen: Instant,
}

#[derive(Default)]
pub struct PeerTable {
    peers: Mutex<HashMap<Uuid, Entry>>,
    manual: Mutex<HashSet<Ipv4Addr>>,
}

impl PeerTable {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Retorna true se a visão pública mudou.
    pub fn upsert(&self, b: &Beacon, ip: Ipv4Addr, now: Instant) -> bool {
        let mut m = self.peers.lock();
        if b.bye {
            return m.remove(&b.id).is_some();
        }
        let info = PeerInfo { id: b.id, nick: b.nick.clone(), status: b.status, ip, ctrl_port: b.ctrl_port };
        match m.get_mut(&b.id) {
            Some(e) => {
                let changed = e.info != info;
                e.info = info;
                e.last_seen = now;
                changed
            }
            None => {
                m.insert(b.id, Entry { info, last_seen: now });
                true
            }
        }
    }

    /// Remove peers expirados; true se algo foi removido.
    pub fn sweep(&self, now: Instant) -> bool {
        let mut m = self.peers.lock();
        let before = m.len();
        m.retain(|_, e| now.duration_since(e.last_seen) < PEER_TTL);
        m.len() != before
    }

    pub fn snapshot(&self) -> Vec<PeerInfo> {
        let mut v: Vec<_> = self.peers.lock().values().map(|e| e.info.clone()).collect();
        v.sort_by(|a, b| a.nick.to_lowercase().cmp(&b.nick.to_lowercase()).then(a.id.cmp(&b.id)));
        v
    }

    pub fn get(&self, id: &Uuid) -> Option<PeerInfo> {
        self.peers.lock().get(id).map(|e| e.info.clone())
    }

    pub fn add_manual(&self, ip: Ipv4Addr) {
        self.manual.lock().insert(ip);
    }

    /// IPs para reanúncio unicast (peers conhecidos + adicionados manualmente).
    pub fn unicast_targets(&self) -> Vec<Ipv4Addr> {
        let mut s: HashSet<Ipv4Addr> = self.manual.lock().clone();
        s.extend(self.peers.lock().values().map(|e| e.info.ip));
        s.into_iter().collect()
    }
}

/// Cria um socket UDP de descoberta vinculado exclusivamente ao IP indicado.
pub fn bind_udp(ip: Ipv4Addr, port: u16) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_broadcast(true)?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(ip, port)).into())?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

pub struct DiscoveryConfig {
    pub bind_ip: Ipv4Addr,
    pub port: u16,
    /// Endereço de broadcast direcionado (ex.: 255.255.255.255). `None` desativa o broadcast.
    pub broadcast: Option<Ipv4Addr>,
    /// Alvos unicast extras (endereço completo; útil em testes com portas diferentes).
    pub extra_targets: Vec<SocketAddr>,
    /// Porta usada nos reanúncios unicast aos peers conhecidos.
    pub unicast_port: u16,
}

pub type OnChange = Arc<dyn Fn(Vec<PeerInfo>) + Send + Sync>;

/// Loop de descoberta. Retorna quando o `shutdown` é acionado (enviando `bye`).
pub async fn run(
    cfg: DiscoveryConfig,
    me: Arc<LocalPeer>,
    table: Arc<PeerTable>,
    on_change: OnChange,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    let sock = Arc::new(UdpSocket::from_std(bind_udp(cfg.bind_ip, cfg.port)?)?);
    let bcast = cfg.broadcast.map(|b| SocketAddr::V4(SocketAddrV4::new(b, cfg.port)));

    let send_all = {
        let sock = sock.clone();
        let table = table.clone();
        let extra = cfg.extra_targets.clone();
        let uport = cfg.unicast_port;
        move |pkt: Vec<u8>| {
            let sock = sock.clone();
            let table = table.clone();
            let extra = extra.clone();
            async move {
                if let Some(b) = bcast {
                    let _ = sock.send_to(&pkt, b).await;
                }
                for t in &extra {
                    let _ = sock.send_to(&pkt, t).await;
                }
                for ip in table.unicast_targets() {
                    let _ = sock.send_to(&pkt, (ip, uport)).await;
                }
            }
        }
    };

    let mut tick = tokio::time::interval(BEACON_INTERVAL);
    let mut sweep = tokio::time::interval(Duration::from_secs(1));
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                send_all(serde_json::to_vec(&me.beacon(false))?).await;
            }
            _ = me.changed.notified() => {
                send_all(serde_json::to_vec(&me.beacon(false))?).await;
                on_change(table.snapshot());
            }
            _ = sweep.tick() => {
                if table.sweep(Instant::now()) { on_change(table.snapshot()); }
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                let SocketAddr::V4(from) = from else { continue };
                let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) else { continue };
                if b.magic != MAGIC || b.id == me.id { continue; }
                let is_new = table.get(&b.id).is_none();
                if table.upsert(&b, *from.ip(), Instant::now()) { on_change(table.snapshot()); }
                if is_new && !b.bye {
                    // peer novo: responde por unicast p/ convergência rápida
                    let pkt = serde_json::to_vec(&me.beacon(false))?;
                    let _ = sock.send_to(&pkt, from).await;
                }
            }
            _ = shutdown.notified() => {
                let pkt = serde_json::to_vec(&me.beacon(true))?;
                send_all(pkt).await;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn beacon(id: Uuid, status: PeerStatus, bye: bool) -> Beacon {
        Beacon { magic: MAGIC.into(), v: 1, id, nick: "x".into(), status, ctrl_port: 1, seq: 1, bye }
    }

    #[test]
    fn table_upsert_expire_bye() {
        let t = PeerTable::new();
        let id = Uuid::new_v4();
        let ip = Ipv4Addr::new(26, 1, 1, 1);
        let t0 = Instant::now();
        assert!(t.upsert(&beacon(id, PeerStatus::Idle, false), ip, t0));
        assert!(!t.upsert(&beacon(id, PeerStatus::Idle, false), ip, t0));
        assert!(t.upsert(&beacon(id, PeerStatus::Streaming, false), ip, t0));
        assert!(!t.sweep(t0 + Duration::from_secs(3)));
        assert!(t.sweep(t0 + Duration::from_secs(7)));
        assert!(t.snapshot().is_empty());
        t.upsert(&beacon(id, PeerStatus::Idle, false), ip, t0);
        assert!(t.upsert(&beacon(id, PeerStatus::Idle, true), ip, t0));
        assert!(t.snapshot().is_empty());
    }

    #[tokio::test]
    async fn two_peers_discover_each_other_on_loopback() {
        let lo = Ipv4Addr::LOCALHOST;
        let (pa, pb) = (47911u16, 47912u16);
        let mk = |port: u16, other: u16, nick: &str| {
            let me = LocalPeer::new(nick.into(), 1);
            let table = PeerTable::new();
            let shutdown = Arc::new(Notify::new());
            let count = Arc::new(AtomicUsize::new(0));
            let c2 = count.clone();
            let cfg = DiscoveryConfig {
                bind_ip: lo, port, broadcast: None,
                extra_targets: vec![SocketAddr::from((lo, other))],
                unicast_port: other,
            };
            let (m, t, s) = (me.clone(), table.clone(), shutdown.clone());
            tokio::spawn(run(cfg, m, t, Arc::new(move |_| { c2.fetch_add(1, Ordering::SeqCst); }), s));
            (me, table, shutdown, count)
        };
        let (_a, ta, sa, _) = mk(pa, pb, "alice");
        let (b, tb, sb, _) = mk(pb, pa, "bob");

        let wait = async {
            loop {
                if ta.snapshot().len() == 1 && tb.snapshot().len() == 1 { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait).await.expect("descoberta falhou");
        assert_eq!(ta.snapshot()[0].nick, "bob");
        assert_eq!(tb.snapshot()[0].nick, "alice");

        // mudança de estado propaga rápido (< 1 s, sem esperar o beacon de 2 s)
        b.set_status(PeerStatus::Streaming);
        let wait = async {
            loop {
                if ta.snapshot()[0].status == PeerStatus::Streaming { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(1), wait).await.expect("status não propagou");

        // bye remove o peer imediatamente
        sb.notify_one();
        let wait = async {
            loop {
                if ta.snapshot().is_empty() { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), wait).await.expect("bye não removeu o peer");
        sa.notify_one();
    }
}
