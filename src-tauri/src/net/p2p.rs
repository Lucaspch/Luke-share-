//! Camada de rede P2P Nativa sobre Iroh (QUIC + STUN + NAT Traversal).
//! Permite conexões seguras ponto-a-ponto via Internet sem necessidade de Radmin VPN.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh::{endpoint::presets, endpoint::Connection, Endpoint, EndpointAddr};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::net::protocol::*;
use crate::net::transport::{parse_nack_payload, RetransmitRing};

pub const LUKE_SHARE_ALPN: &[u8] = b"luke-share/1.0";

/// Ticket de convite de sala compartilhável entre amigos.
/// Contém o identificador criptográfico do nó e suas dicas de roteamento (relays e endereços diretos).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RoomTicket {
    pub raw: String,
}

impl RoomTicket {
    pub fn new(addr: &EndpointAddr) -> Result<Self> {
        let json = serde_json::to_string(addr)?;
        let b64 = crate::pipeline::base64_encode(json.as_bytes());
        Ok(Self { raw: format!("lk-{}", b64) })
    }

    pub fn to_endpoint_addr(&self) -> Result<EndpointAddr> {
        let trimmed = self.raw.strip_prefix("lk-").unwrap_or(&self.raw);
        let bytes = base64_decode(trimmed)?;
        let addr: EndpointAddr = serde_json::from_slice(&bytes)?;
        Ok(addr)
    }
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;

    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => anyhow::bail!("caractere base64 inválido: {}", b as char),
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            v.push((buf >> bits) as u8);
        }
    }
    Ok(v)
}

/// Cria um Endpoint Iroh configurado com presets N0 (STUN + Relays públicos globais).
pub async fn create_endpoint() -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![LUKE_SHARE_ALPN.to_vec()])
        .bind()
        .await?;
    Ok(endpoint)
}

/// Representa a conexão P2P de um espectador no transmissor (Host).
#[derive(Clone)]
pub struct P2pViewer {
    pub peer_id: Uuid,
    pub nick: String,
    pub conn: Connection,
    pub tx_ctrl: mpsc::Sender<Control>,
}

impl P2pViewer {
    pub async fn send_control(&self, msg: Control) -> Result<()> {
        self.tx_ctrl.send(msg).await.map_err(|_| anyhow!("canal de controle encerrado"))
    }

    pub fn send_datagram(&self, data: Bytes) -> Result<()> {
        self.conn.send_datagram(data).map_err(|e| anyhow!("{e}"))
    }
}

/// Gerenciador de conexões P2P no lado transmissor (Host).
#[derive(Clone)]
pub struct P2pHostManager {
    viewers: Arc<Mutex<HashMap<Uuid, P2pViewer>>>,
    relay_candidates: Arc<Mutex<HashMap<Uuid, (String, Connection)>>>,
}

impl P2pHostManager {
    pub fn new() -> Self {
        Self {
            viewers: Arc::new(Mutex::new(HashMap::new())),
            relay_candidates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register_relay(&self, peer_id: Uuid, ticket: String, conn: Connection) -> Vec<Connection> {
        let mut rc = self.relay_candidates.lock();
        let other_conns: Vec<Connection> = rc.iter().filter(|(id, _)| **id != peer_id).map(|(_, (_, c))| c.clone()).collect();
        rc.insert(peer_id, (ticket, conn));
        other_conns
    }

    pub fn unregister_relay(&self, peer_id: &Uuid) {
        self.relay_candidates.lock().remove(peer_id);
    }

    pub fn get_other_relay_tickets(&self, exclude: &Uuid) -> Vec<String> {
        self.relay_candidates
            .lock()
            .iter()
            .filter(|(id, _)| *id != exclude)
            .map(|(_, (t, _))| t.clone())
            .collect()
    }

    pub fn add_viewer(&self, viewer: P2pViewer) {
        self.viewers.lock().insert(viewer.peer_id, viewer);
    }

    pub fn remove_viewer(&self, peer_id: &Uuid) {
        self.viewers.lock().remove(peer_id);
    }

    pub fn viewers_count(&self) -> usize {
        self.viewers.lock().len()
    }

    pub fn viewers_info(&self) -> Vec<(Uuid, String)> {
        self.viewers.lock().values().map(|v| (v.peer_id, v.nick.clone())).collect()
    }

    /// Retorna uma cópia da lista de espectadores ativos para despacho round-robin com micro-pacing.
    pub fn active_viewers(&self) -> Vec<P2pViewer> {
        self.viewers.lock().values().cloned().collect()
    }

    /// Envia datagramas de mídia para todos os espectadores conectados.
    pub fn broadcast_datagram(&self, data: Bytes) {
        let viewers = self.viewers.lock();
        for v in viewers.values() {
            let _ = v.send_datagram(data.clone());
        }
    }

    /// Envia mensagem de controle para todos os espectadores conectados.
    pub async fn broadcast_control(&self, msg: Control) {
        let viewers: Vec<_> = self.viewers.lock().values().map(|v| v.tx_ctrl.clone()).collect();
        for tx in viewers {
            let _ = tx.send(msg.clone()).await;
        }
    }

    /// Transmite a lista e contagem atualizada de espectadores para todos os conectados via P2P.
    pub async fn broadcast_viewers(&self) {
        let list: Vec<String> = self.viewers.lock().values().map(|v| v.nick.clone()).collect();
        let count = list.len() as u32;
        self.broadcast_control(Control::ViewersUpdate { count, viewers: list }).await;
    }

    /// Inicia a escuta de conexões Iroh em segundo plano, servindo NACKs do RetransmitRing e PLIs.
    pub fn start_listener(
        self: Arc<Self>,
        endpoint: Endpoint,
        offer_video: VideoOffer,
        offer_audio: Option<AudioOffer>,
        ring: Arc<Mutex<RetransmitRing>>,
        keyframe_requester: Arc<dyn Fn() + Send + Sync>,
        nacks_served: Arc<std::sync::atomic::AtomicU64>,
        on_telemetry: Arc<dyn Fn(TelemetryReport) + Send + Sync>,
        stop: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let last_pli = Arc::new(parking_lot::Mutex::new(std::time::Instant::now() - Duration::from_secs(5)));
            let pending_pli = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // Tarefa para PLI coalescing (250ms)
            let stop_pli = stop.clone();
            let last_pli_task = last_pli.clone();
            let pending_pli_task = pending_pli.clone();
            let kf_req_task = keyframe_requester.clone();
            tokio::spawn(async move {
                while !stop_pli.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if pending_pli_task.load(Ordering::Relaxed) {
                        let now = std::time::Instant::now();
                        let elapsed = {
                            let lp = last_pli_task.lock();
                            now.duration_since(*lp)
                        };
                        if elapsed >= Duration::from_millis(250) {
                            *last_pli_task.lock() = now;
                            pending_pli_task.store(false, Ordering::Relaxed);
                            tracing::info!("p2p_host: disparando IDR acumulado por PLI coalescing");
                            kf_req_task();
                        }
                    }
                }
            });

            while !stop.load(Ordering::SeqCst) {
                tokio::select! {
                    incoming = endpoint.accept() => {
                        let Some(incoming) = incoming else { break };
                        let this = self.clone();
                        let offer_v = offer_video.clone();
                        let offer_a = offer_audio.clone();
                        let ring = ring.clone();
                        let nacks_cnt = nacks_served.clone();
                        let kf_req = keyframe_requester.clone();
                        let last_pli = last_pli.clone();
                        let pending_pli = pending_pli.clone();
                        let on_telem = on_telemetry.clone();
                        let stop_viewer = stop.clone();

                        tokio::spawn(async move {
                            let conn = match incoming.await {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::warn!("p2p_host: falha no handshake de conexão Iroh: {e:#}");
                                    return;
                                }
                            };

                            let (mut send_ctrl, mut recv_ctrl) = match conn.accept_bi().await {
                                Ok(s) => s,
                                Err(e) => {
                                    tracing::warn!("p2p_host: falha ao aceitar stream de controle: {e:#}");
                                    return;
                                }
                            };

                            let msg = match read_control(&mut recv_ctrl).await {
                                Ok(m) => m,
                                Err(e) => {
                                    tracing::warn!("p2p_host: falha ao ler Watch inicial: {e:#}");
                                    return;
                                }
                            };

                            let (peer_id, nick) = match msg {
                                Control::Watch { peer_id, nick, .. } => (peer_id, nick),
                                _ => return,
                            };

                            tracing::info!("p2p_host: espectador P2P conectado: {nick} ({peer_id})");

                            let accept_msg = Control::Accept {
                                video: offer_v,
                                audio: offer_a,
                                udp_port: 0,
                            };
                            if let Err(e) = write_control(&mut send_ctrl, &accept_msg).await {
                                tracing::warn!("p2p_host: erro ao enviar Accept: {e:#}");
                                return;
                            }

                            // Dispara keyframe inicial para o novo espectador
                            kf_req();

                            let (tx_ctrl, mut rx_ctrl) = mpsc::channel::<Control>(32);
                            let viewer = P2pViewer {
                                peer_id,
                                nick: nick.clone(),
                                conn: conn.clone(),
                                tx_ctrl,
                            };
                            this.add_viewer(viewer);
                            this.broadcast_viewers().await;

                            // Tarefa A: Envio de controle para o espectador
                            let send_task = tokio::spawn(async move {
                                while let Some(msg) = rx_ctrl.recv().await {
                                    if write_control(&mut send_ctrl, &msg).await.is_err() {
                                        break;
                                    }
                                }
                            });

                            // Tarefa B: Escuta de datagramas do espectador (NACKs e PLIs)
                            let conn_dg = conn.clone();
                            let this_dg = this.clone();
                            let recv_dg_task = tokio::spawn(async move {
                                while !stop_viewer.load(Ordering::SeqCst) {
                                    match conn_dg.read_datagram().await {
                                        Ok(dg) => {
                                            if let Some((h, payload)) = MediaHeader::parse(&dg) {
                                                match h.kind {
                                                    KIND_NACK => {
                                                        let missing = parse_nack_payload(payload);
                                                        let to_resend: Vec<Arc<Vec<u8>>> = {
                                                            let r = ring.lock();
                                                            missing
                                                                .into_iter()
                                                                .filter_map(|idx| r.get(h.frame_id, idx))
                                                                .collect()
                                                        };
                                                        let count = to_resend.len();
                                                        for pkt in to_resend {
                                                            let _ = conn_dg.send_datagram(Bytes::copy_from_slice(&pkt));
                                                        }
                                                        nacks_cnt.fetch_add(count as u64, Ordering::Relaxed);
                                                    }
                                                    KIND_PLI => {
                                                        let now = std::time::Instant::now();
                                                        let elapsed = {
                                                            let lp = last_pli.lock();
                                                            now.duration_since(*lp)
                                                        };
                                                        if elapsed >= Duration::from_millis(250) {
                                                            *last_pli.lock() = now;
                                                            pending_pli.store(false, Ordering::Relaxed);
                                                            tracing::info!("p2p_host: PLI recebido de {peer_id}, gerando IDR imediato");
                                                            kf_req();
                                                        } else {
                                                            pending_pli.store(true, Ordering::Relaxed);
                                                            tracing::debug!("p2p_host: PLI de {peer_id} agrupado (coalescing)");
                                                        }
                                                    }
                                                    KIND_TELEMETRY => {
                                                        if let Some(rep) = TelemetryReport::from_bytes(payload) {
                                                            on_telem(rep);
                                                        }
                                                    }
                                                    KIND_RELAY_ANNOUNCE => {
                                                        if let Ok(ticket_str) = std::str::from_utf8(payload) {
                                                            if ticket_str.starts_with("lk-") {
                                                                let others = this_dg.register_relay(peer_id, ticket_str.to_string(), conn_dg.clone());
                                                                tracing::info!("p2p_host: candidato mesh registrado para {peer_id} (outros ativos: {})", others.len());
                                                                let fwd_pkt = control_packet(KIND_RELAY_ANNOUNCE, 0, ticket_str.as_bytes());
                                                                let b_fwd = Bytes::from(fwd_pkt);
                                                                for oc in &others {
                                                                    let _ = oc.send_datagram(b_fwd.clone());
                                                                }
                                                                for ot in this_dg.get_other_relay_tickets(&peer_id) {
                                                                    let fwd_back = control_packet(KIND_RELAY_ANNOUNCE, 0, ot.as_bytes());
                                                                    let _ = conn_dg.send_datagram(Bytes::from(fwd_back));
                                                                }
                                                            }
                                                        }
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        Err(_) => break, // espectador desconectou
                                    }
                                }
                            });

                            // Tarefa C: Escuta de controle do espectador (ex: Bye)
                            let recv_ctrl_task = tokio::spawn(async move {
                                loop {
                                    match read_control(&mut recv_ctrl).await {
                                        Ok(Control::Bye) => break,
                                        Err(_) => {
                                            // Stream de controle finalizada pelo espectador (não desconecta a sessão)
                                            std::future::pending::<()>().await;
                                        }
                                        _ => {}
                                    }
                                }
                            });

                            // Aguarda desconexão real da conexão QUIC ou Bye voluntário
                            let conn_close_wait = conn.clone();
                            tokio::select! {
                                _ = conn_close_wait.closed() => {
                                    tracing::info!("p2p_host: conexão QUIC de {nick} ({peer_id}) foi encerrada");
                                }
                                _ = recv_ctrl_task => {
                                    tracing::info!("p2p_host: espectador {nick} ({peer_id}) enviou Bye voluntário");
                                }
                            }

                            send_task.abort();
                            recv_dg_task.abort();
                            this.remove_viewer(&peer_id);
                            this.unregister_relay(&peer_id);
                            this.broadcast_viewers().await;
                            tracing::info!("p2p_host: espectador P2P desconectado: {nick} ({peer_id})");
                        });
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::protocol::{read_control, write_control, VideoOffer};

    #[tokio::test]
    async fn test_create_endpoint_and_ticket() -> Result<()> {
        let ep = create_endpoint().await?;
        let addr = ep.addr();
        let ticket = RoomTicket::new(&addr)?;
        assert!(ticket.raw.starts_with("lk-"));

        let recovered = ticket.to_endpoint_addr()?;
        assert_eq!(addr.id, recovered.id);
        Ok(())
    }

    #[tokio::test]
    async fn test_p2p_connection_control_and_datagrams() -> Result<()> {
        // 1. Host inicia Endpoint
        let host_ep = create_endpoint().await?;
        let host_addr = host_ep.addr();
        let ticket = RoomTicket::new(&host_addr)?;

        // 2. Viewer inicia Endpoint e conecta ao host usando o ticket
        let viewer_ep = create_endpoint().await?;
        let target_addr = ticket.to_endpoint_addr()?;

        let (tx_done, rx_done) = tokio::sync::oneshot::channel::<()>();

        let host_handle = tokio::spawn(async move {
            let incoming = host_ep.accept().await.expect("host accept");
            let conn = incoming.await.expect("host conn");

            // Aceita bi-stream de controle
            let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi");
            let msg = read_control(&mut recv).await.expect("read_control");
            match msg {
                Control::Watch { nick, .. } => {
                    assert_eq!(nick, "LucasViewer");
                    let accept_msg = Control::Accept {
                        video: VideoOffer { width: 1920, height: 1080, fps: 60, codec: "h264".into() },
                        audio: None,
                        udp_port: 0,
                    };
                    write_control(&mut send, &accept_msg).await.expect("write_control");
                }
                _ => panic!("esperava Watch"),
            }

            // Envia datagrama de mídia
            conn.send_datagram(Bytes::from_static(b"MEDIA_PACKET_FRAME_1")).expect("send_datagram");

            // Aguarda o viewer concluir antes de fechar a conexão
            let _ = rx_done.await;
        });

        let viewer_handle = tokio::spawn(async move {
            let conn = viewer_ep.connect(target_addr, LUKE_SHARE_ALPN).await.expect("viewer connect");

            // Abre bi-stream de controle
            let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
            let watch_msg = Control::Watch {
                peer_id: Uuid::new_v4(),
                nick: "LucasViewer".into(),
                udp_port: 0,
            };
            write_control(&mut send, &watch_msg).await.expect("write_control");

            let resp = read_control(&mut recv).await.expect("read_control resp");
            match resp {
                Control::Accept { video, .. } => {
                    assert_eq!(video.width, 1920);
                }
                _ => panic!("esperava Accept"),
            }

            // Recebe datagrama de mídia
            let dg = conn.read_datagram().await.expect("read_datagram");
            assert_eq!(&dg[..], b"MEDIA_PACKET_FRAME_1");

            let _ = tx_done.send(());
        });

        let (r1, r2) = tokio::join!(host_handle, viewer_handle);
        r1.unwrap();
        r2.unwrap();

        Ok(())
    }

    #[tokio::test]
    async fn test_host_manager_listener_lifecycle() -> Result<()> {
        let host_ep = create_endpoint().await?;
        let host_addr = host_ep.addr();
        let host_mgr = Arc::new(P2pHostManager::new());
        let ring = Arc::new(parking_lot::Mutex::new(RetransmitRing::new(100)));
        let stop = Arc::new(AtomicBool::new(false));
        let nacks = Arc::new(std::sync::atomic::AtomicU64::new(0));

        host_mgr.clone().start_listener(
            host_ep,
            VideoOffer { width: 1920, height: 1080, fps: 60, codec: "h264".into() },
            None,
            ring,
            Arc::new(|| {}),
            nacks,
            Arc::new(|_| {}),
            stop.clone(),
        );

        let viewer_ep = create_endpoint().await?;
        let conn = viewer_ep.connect(host_addr, LUKE_SHARE_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let watch_msg = Control::Watch {
            peer_id: Uuid::new_v4(),
            nick: "ViewerTest".into(),
            udp_port: 0,
        };
        write_control(&mut send, &watch_msg).await?;
        let resp = read_control(&mut recv).await?;
        assert!(matches!(resp, Control::Accept { .. }));

        // Aguarda 50ms
        tokio::time::sleep(Duration::from_millis(50)).await;
        let count_before = host_mgr.viewers_count();
        eprintln!("Viewers with send stream open: {count_before}");

        // Agora simula o que acontecia: send_ctrl é dropped!
        drop(send);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let count_after = host_mgr.viewers_count();
        eprintln!("Viewers with send stream DROPPED: {count_after}");

        assert_eq!(count_after, 1, "O espectador NÃO pode ser removido apenas porque o send_ctrl fechou!");

        stop.store(true, Ordering::SeqCst);
        Ok(())
    }
}
