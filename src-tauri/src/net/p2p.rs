//! Camada de rede P2P Nativa sobre Iroh (QUIC + STUN + NAT Traversal + Cascateamento Mesh Autenticado).
//! Permite conexões seguras ponto-a-ponto via Internet, isolamento de congestionamento por espectador
//! e retransmissão assistida entre espectadores de fibra quando 3+ pares entram na sala.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh::{endpoint::presets, endpoint::Connection, Endpoint, EndpointAddr};
use parking_lot::Mutex;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::net::protocol::*;
use crate::net::transport::{parse_nack_payload, RetransmitRing};

pub const LUKE_SHARE_ALPN: &[u8] = b"luke-share/1.0";
pub const LUKE_MESH_ALPN: &[u8] = b"luke-share-mesh/1.0";

/// Ticket de convite de sala compartilhável (`lk-...`).
/// Contém o identificador criptográfico do nó e suas dicas de roteamento (relays e endereços diretos).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RoomTicket {
    pub raw: String,
}

impl RoomTicket {
    pub fn new(addr: &EndpointAddr) -> Result<Self> {
        let json = serde_json::to_string(addr)?;
        let b64 = crate::pipeline::base64_encode(json.as_bytes());
        Ok(Self {
            raw: format!("lk-{}", b64),
        })
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

/// Cria um Endpoint Iroh configurado com presets N0 (STUN + Pkarr DNS + Relays públicos globais).
pub async fn create_endpoint() -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![LUKE_SHARE_ALPN.to_vec(), LUKE_MESH_ALPN.to_vec()])
        .bind()
        .await?;
    Ok(endpoint)
}

#[derive(Clone, Debug)]
pub struct ViewerNetHealth {
    pub rtt_ms: u32,
    pub loss_pct: f32,
    pub updated_at: Instant,
    pub mesh_ticket: Option<String>,
    pub downstream_count: usize,
    pub assigned_relay: Option<Uuid>,
}

impl Default for ViewerNetHealth {
    fn default() -> Self {
        Self {
            rtt_ms: 35,
            loss_pct: 0.0,
            updated_at: Instant::now(),
            mesh_ticket: None,
            downstream_count: 0,
            assigned_relay: None,
        }
    }
}

/// Representa a conexão P2P de um espectador no transmissor (Host).
#[derive(Clone)]
pub struct P2pViewer {
    pub peer_id: Uuid,
    pub nick: String,
    pub conn: Connection,
    pub tx_ctrl: mpsc::Sender<Control>,
    /// Ativado quando apenas este espectador sofre perda isolada de download (recebe paridade FEC extra).
    pub needs_extra_fec: Arc<AtomicBool>,
    /// Quando `Some(relay_id)`, este espectador já está recebendo datagramas autenticados via cascata de outro viewer.
    pub cascaded_via: Arc<Mutex<Option<Uuid>>>,
}

impl P2pViewer {
    pub async fn send_control(&self, msg: Control) -> Result<()> {
        self.tx_ctrl
            .send(msg)
            .await
            .map_err(|_| anyhow!("canal de controle encerrado"))
    }

    pub fn try_send_control(&self, msg: Control) {
        let _ = self.tx_ctrl.try_send(msg);
    }

    pub fn send_datagram(&self, data: Bytes) -> Result<()> {
        self.conn.send_datagram(data).map_err(|e| anyhow!("{e}"))
    }

    pub fn is_cascaded(&self) -> bool {
        self.cascaded_via.lock().is_some()
    }

    pub fn wants_extra_fec(&self) -> bool {
        self.needs_extra_fec.load(Ordering::Relaxed)
    }
}

/// Resultado da avaliação agregada de congestionamento de todos os espectadores.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClusterTelemetryDecision {
    /// RTT efetivo que deve alimentar o `CongestionController` global do encoder.
    pub global_rtt_ms: u32,
    /// Perda efetiva que deve alimentar o `CongestionController` global (isolando quedas individuais de download).
    pub global_loss_pct: f32,
    /// `true` se o gargalo aparenta ser o upload do Host (maioria dos viewers com perda simultânea).
    pub host_upload_congested: bool,
    /// Quantidade de espectadores com perda isolada apenas na rota deles.
    pub isolated_degraded_viewers: usize,
}

/// Avalia telemetrias por espectador diferenciando gargalo de upload do Host vs queda no download de 1 espectador.
pub fn evaluate_cluster_telemetry(samples: &[(Uuid, u32, f32)]) -> Option<ClusterTelemetryDecision> {
    if samples.is_empty() {
        return None;
    }
    if samples.len() == 1 {
        let (_, rtt, loss) = samples[0];
        return Some(ClusterTelemetryDecision {
            global_rtt_ms: rtt,
            global_loss_pct: loss,
            host_upload_congested: loss >= 2.2,
            isolated_degraded_viewers: 0,
        });
    }

    let degraded: Vec<&(Uuid, u32, f32)> = samples.iter().filter(|(_, _, loss)| *loss >= 2.2).collect();
    let healthy: Vec<&(Uuid, u32, f32)> = samples.iter().filter(|(_, _, loss)| *loss < 2.2).collect();

    // Se metade ou mais dos espectadores sofrem perda simultânea, o gargalo é o upload do Host!
    if degraded.len() * 2 >= samples.len() {
        let avg_rtt = (degraded.iter().map(|(_, r, _)| *r as u64).sum::<u64>() / degraded.len() as u64) as u32;
        let avg_loss = degraded.iter().map(|(_, _, l)| *l).sum::<f32>() / degraded.len() as f32;
        Some(ClusterTelemetryDecision {
            global_rtt_ms: avg_rtt,
            global_loss_pct: avg_loss,
            host_upload_congested: true,
            isolated_degraded_viewers: 0,
        })
    } else {
        // Apenas 1 minoria isolada está perdendo pacotes: NÃO derruba a qualidade de vídeo de quem tem internet boa!
        let avg_rtt = (healthy.iter().map(|(_, r, _)| *r as u64).sum::<u64>() / healthy.len().max(1) as u64) as u32;
        let avg_loss = healthy.iter().map(|(_, _, l)| *l).sum::<f32>() / healthy.len().max(1) as f32;
        Some(ClusterTelemetryDecision {
            global_rtt_ms: avg_rtt,
            global_loss_pct: avg_loss,
            host_upload_congested: false,
            isolated_degraded_viewers: degraded.len(),
        })
    }
}

/// Gerenciador de conexões P2P no lado transmissor (Host).
#[derive(Clone)]
pub struct P2pHostManager {
    pub stream_token: [u8; 16],
    viewers: Arc<Mutex<HashMap<Uuid, P2pViewer>>>,
    health: Arc<Mutex<HashMap<Uuid, ViewerNetHealth>>>,
}

impl P2pHostManager {
    pub fn new() -> Self {
        let mut token = [0u8; 16];
        rand::rng().fill(&mut token);
        if token == [0u8; 16] {
            token[0] = 0x5A;
        }
        Self {
            stream_token: token,
            viewers: Arc::new(Mutex::new(HashMap::new())),
            health: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register_relay(&self, peer_id: Uuid, ticket: String) {
        let mut h = self.health.lock();
        let entry = h.entry(peer_id).or_default();
        entry.mesh_ticket = Some(ticket);
        drop(h);
        self.rebalance_relay_mesh();
    }

    pub fn add_viewer(&self, viewer: P2pViewer) {
        let id = viewer.peer_id;
        self.viewers.lock().insert(id, viewer);
        self.health.lock().entry(id).or_default();
    }

    pub fn remove_viewer(&self, peer_id: &Uuid) {
        self.viewers.lock().remove(peer_id);
        let mut h = self.health.lock();
        let prev = h.remove(peer_id);
        if let Some(removed) = prev {
            if let Some(up_id) = removed.assigned_relay {
                if let Some(up) = h.get_mut(&up_id) {
                    up.downstream_count = up.downstream_count.saturating_sub(1);
                }
            }
        }
        // Se o espectador removido era retransmissor de alguém, restaura envio direto imediatamente!
        let mut orphaned = Vec::new();
        for (vid, vh) in h.iter_mut() {
            if vh.assigned_relay == Some(*peer_id) {
                vh.assigned_relay = None;
                orphaned.push(*vid);
            }
        }
        drop(h);

        if !orphaned.is_empty() {
            let v_lock = self.viewers.lock();
            for vid in orphaned {
                if let Some(v) = v_lock.get(&vid) {
                    *v.cascaded_via.lock() = None;
                }
            }
        }
        self.rebalance_relay_mesh();
    }

    /// Confirma ou desfaz o estado de cascata P2P reportado pelo espectador downstream.
    pub fn set_viewer_cascade_status(&self, viewer_id: Uuid, relay_id: Uuid, connected: bool) {
        if let Some(v) = self.viewers.lock().get(&viewer_id) {
            if connected {
                *v.cascaded_via.lock() = Some(relay_id);
                tracing::info!("p2p_host: viewer {viewer_id} em cascata autenticada via {relay_id} (alívio de upload ativo)");
            } else {
                *v.cascaded_via.lock() = None;
                tracing::info!("p2p_host: cascata de {viewer_id} encerrada, retomando envio direto imediato");
            }
        }
        if !connected {
            let mut h = self.health.lock();
            if let Some(vh) = h.get_mut(&viewer_id) {
                if vh.assigned_relay == Some(relay_id) {
                    vh.assigned_relay = None;
                }
            }
            if let Some(up) = h.get_mut(&relay_id) {
                up.downstream_count = up.downstream_count.saturating_sub(1);
            }
        }
    }

    /// Registra telemetria individual de um espectador, aplica isolamento de congestionamento
    /// e aciona cascateamento Mesh quando há 3+ espectadores ou rota degradada isolada.
    pub fn record_viewer_telemetry(&self, peer_id: Uuid, rep: TelemetryReport) -> Option<ClusterTelemetryDecision> {
        let loss = rep.loss_pct_x10 as f32 / 10.0;
        let rtt = rep.rtt_ms as u32;

        let samples: Vec<(Uuid, u32, f32)> = {
            let mut h = self.health.lock();
            let entry = h.entry(peer_id).or_default();
            entry.rtt_ms = rtt;
            entry.loss_pct = loss;
            entry.updated_at = Instant::now();

            h.iter()
                .filter(|(_, v)| v.updated_at.elapsed() < Duration::from_secs(5))
                .map(|(id, v)| (*id, v.rtt_ms, v.loss_pct))
                .collect()
        };

        let decision = evaluate_cluster_telemetry(&samples)?;

        // Atualiza flag individual `needs_extra_fec` apenas nos espectadores com perda isolada
        {
            let v_lock = self.viewers.lock();
            if let Some(v) = v_lock.get(&peer_id) {
                let isolated_bad = !decision.host_upload_congested && samples.len() >= 2 && loss >= 2.2;
                v.needs_extra_fec.store(isolated_bad, Ordering::Relaxed);
            }
        }

        if self.viewers_count() >= 3 || decision.isolated_degraded_viewers > 0 {
            self.rebalance_relay_mesh();
        }

        Some(decision)
    }

    /// Quando 3+ espectadores estão na sala (ou alguém sofre perda isolada de rota),
    /// seleciona espectadores saudáveis de baixa latência ("fibra rápida") para retransmitir
    /// datagramas autenticados aos novos pares.
    pub fn rebalance_relay_mesh(&self) {
        let viewers_map = self.viewers.lock().clone();
        let total = viewers_map.len();
        if total < 2 {
            return;
        }

        let mut h = self.health.lock();

        // Candidatos a retransmissor (upstream): não estão em cascata, têm ticket mesh, perda < 1.2%, RTT < 90ms e < 2 filhos
        let mut candidates: Vec<(Uuid, u32, String)> = h
            .iter()
            .filter(|(_, vh)| {
                vh.assigned_relay.is_none()
                    && vh.mesh_ticket.is_some()
                    && vh.loss_pct < 1.2
                    && vh.rtt_ms <= 90
                    && vh.downstream_count < 2
            })
            .map(|(id, vh)| (*id, vh.rtt_ms, vh.mesh_ticket.clone().unwrap()))
            .collect();

        candidates.sort_by_key(|(_, rtt, _)| *rtt);
        if candidates.is_empty() {
            return;
        }

        // Mantém pelo menos 2 espectadores diretos no Host; do 3º em diante (ou espectador com perda isolada >= 3%),
        // delega cascata autenticada a um candidato saudável!
        let direct_non_cascaded = h.values().filter(|vh| vh.assigned_relay.is_none()).count();

        let targets: Vec<Uuid> = h
            .iter()
            .filter(|(id, vh)| {
                vh.assigned_relay.is_none()
                    && vh.downstream_count == 0
                    && candidates.first().map(|(cid, _, _)| cid != *id).unwrap_or(false)
                    && ((total >= 3 && direct_non_cascaded > 2) || vh.loss_pct >= 3.0)
            })
            .map(|(id, _)| *id)
            .collect();

        for target_id in targets {
            let Some((relay_id, _, relay_ticket)) = candidates.iter().find(|(cid, _, _)| *cid != target_id).cloned() else {
                continue;
            };
            let cur_down = h.get(&relay_id).map(|u| u.downstream_count).unwrap_or(2);
            if cur_down >= 2 {
                continue;
            }

            if let (Some(relay_v), Some(target_v)) = (viewers_map.get(&relay_id), viewers_map.get(&target_id)) {
                if let Some(up_h) = h.get_mut(&relay_id) {
                    up_h.downstream_count += 1;
                }
                if let Some(tg_h) = h.get_mut(&target_id) {
                    tg_h.assigned_relay = Some(relay_id);
                }

                relay_v.try_send_control(Control::RelayRole { active: true });
                target_v.try_send_control(Control::RelayAssign {
                    relay_ticket,
                    relay_peer_id: relay_id,
                    stream_token: self.stream_token,
                });
                tracing::info!("p2p_host: atribuindo relay mesh {relay_id} -> {target_id}");
                break; // Atribui gradualmente 1 por ciclo para estabilidade
            }
        }
    }

    pub fn viewers_count(&self) -> usize {
        self.viewers.lock().len()
    }

    pub fn viewers_info(&self) -> Vec<(Uuid, String)> {
        self.viewers
            .lock()
            .values()
            .map(|v| (v.peer_id, v.nick.clone()))
            .collect()
    }

    pub fn active_viewers(&self) -> Vec<P2pViewer> {
        self.viewers.lock().values().cloned().collect()
    }

    pub async fn broadcast_control(&self, msg: Control) {
        let viewers: Vec<_> = self.viewers.lock().values().map(|v| v.tx_ctrl.clone()).collect();
        for tx in viewers {
            let _ = tx.send(msg.clone()).await;
        }
    }

    pub async fn broadcast_viewers(&self) {
        let list: Vec<String> = self.viewers.lock().values().map(|v| v.nick.clone()).collect();
        let count = list.len() as u32;
        self.broadcast_control(Control::ViewersUpdate { count, viewers: list })
            .await;
    }

    /// Inicia a escuta de conexões Iroh em segundo plano, servindo NACKs, PLIs, Telemetria Isolada e Cascata Mesh.
    pub fn start_listener(
        self: Arc<Self>,
        endpoint: Endpoint,
        offer_video: VideoOffer,
        offer_audio: Option<AudioOffer>,
        ring: Arc<Mutex<RetransmitRing>>,
        keyframe_requester: Arc<dyn Fn() + Send + Sync>,
        nacks_served: Arc<std::sync::atomic::AtomicU64>,
        on_cluster_telemetry: Arc<dyn Fn(ClusterTelemetryDecision) + Send + Sync>,
        stop: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let last_pli = Arc::new(parking_lot::Mutex::new(Instant::now() - Duration::from_secs(5)));
            let pending_pli = Arc::new(AtomicBool::new(false));

            // Coalescing de PLI (250ms)
            let stop_pli = stop.clone();
            let last_pli_task = last_pli.clone();
            let pending_pli_task = pending_pli.clone();
            let kf_req_task = keyframe_requester.clone();
            tokio::spawn(async move {
                while !stop_pli.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if pending_pli_task.load(Ordering::Relaxed) {
                        let now = Instant::now();
                        let elapsed = {
                            let lp = last_pli_task.lock();
                            now.duration_since(*lp)
                        };
                        if elapsed >= Duration::from_millis(250) {
                            *last_pli_task.lock() = now;
                            pending_pli_task.store(false, Ordering::Relaxed);
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
                        let on_telem = on_cluster_telemetry.clone();
                        let stop_viewer = stop.clone();

                        tokio::spawn(async move {
                            let conn = match incoming.await {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::warn!("p2p_host: falha no handshake Iroh: {e:#}");
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
                                Err(_) => return,
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
                                stream_token: Some(this.stream_token),
                            };
                            if write_control(&mut send_ctrl, &accept_msg).await.is_err() {
                                return;
                            }

                            kf_req();

                            let (tx_ctrl, mut rx_ctrl) = mpsc::channel::<Control>(32);
                            let viewer = P2pViewer {
                                peer_id,
                                nick: nick.clone(),
                                conn: conn.clone(),
                                tx_ctrl,
                                needs_extra_fec: Arc::new(AtomicBool::new(false)),
                                cascaded_via: Arc::new(Mutex::new(None)),
                            };
                            this.add_viewer(viewer);
                            this.broadcast_viewers().await;

                            let send_task = tokio::spawn(async move {
                                while let Some(msg) = rx_ctrl.recv().await {
                                    if write_control(&mut send_ctrl, &msg).await.is_err() {
                                        break;
                                    }
                                }
                            });

                            let conn_dg = conn.clone();
                            let this_dg = this.clone();
                            let kf_req_dg = kf_req.clone();
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
                                                        // Se o espectador pediu PLI emergencial, garante envio direto imediato caso cascata tenha falhado
                                                        let now = Instant::now();
                                                        let elapsed = {
                                                            let lp = last_pli.lock();
                                                            now.duration_since(*lp)
                                                        };
                                                        if elapsed >= Duration::from_millis(250) {
                                                            *last_pli.lock() = now;
                                                            pending_pli.store(false, Ordering::Relaxed);
                                                            kf_req_dg();
                                                        } else {
                                                            pending_pli.store(true, Ordering::Relaxed);
                                                        }
                                                    }
                                                    KIND_TELEMETRY => {
                                                        if let Some(rep) = TelemetryReport::from_bytes(payload) {
                                                            if let Some(decision) = this_dg.record_viewer_telemetry(peer_id, rep) {
                                                                on_telem(decision);
                                                            }
                                                        }
                                                    }
                                                    KIND_RELAY_ANNOUNCE => {
                                                        if let Ok(ticket_str) = std::str::from_utf8(payload) {
                                                            if ticket_str.starts_with("lk-") {
                                                                this_dg.register_relay(peer_id, ticket_str.to_string());
                                                            }
                                                        }
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                            });

                            let this_ctrl = this.clone();
                            let kf_req_ctrl = kf_req.clone();
                            let recv_ctrl_task = tokio::spawn(async move {
                                loop {
                                    match read_control(&mut recv_ctrl).await {
                                        Ok(Control::Bye) => break,
                                        Ok(Control::RelayStatus { connected, relay_peer_id }) => {
                                            this_ctrl.set_viewer_cascade_status(peer_id, relay_peer_id, connected);
                                            if !connected {
                                                kf_req_ctrl();
                                            }
                                        }
                                        Err(_) => {
                                            std::future::pending::<()>().await;
                                        }
                                        _ => {}
                                    }
                                }
                            });

                            let conn_close_wait = conn.clone();
                            tokio::select! {
                                _ = conn_close_wait.closed() => {}
                                _ = recv_ctrl_task => {}
                            }

                            send_task.abort();
                            recv_dg_task.abort();
                            this.remove_viewer(&peer_id);
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

    #[test]
    fn isolated_viewer_loss_does_not_penalize_global_bitrate() {
        let v1 = Uuid::new_v4();
        let v2 = Uuid::new_v4();
        let v3 = Uuid::new_v4();

        // 3 espectadores: v1 e v2 estão com 0% de perda (fibra saudável), v3 está com 12% de perda no Wi-Fi dele
        let samples = vec![(v1, 18, 0.0), (v2, 24, 0.2), (v3, 95, 12.0)];
        let dec = evaluate_cluster_telemetry(&samples).unwrap();
        assert!(
            !dec.host_upload_congested,
            "Perda isolada de 1 espectador em 3 não deve ser tratada como gargalo de upload do Host!"
        );
        assert!(dec.global_loss_pct < 0.5, "Bitrate global do Host deve preservar qualidade p/ v1 e v2!");
        assert_eq!(dec.isolated_degraded_viewers, 1);

        // Agora 2 de 3 espectadores sofrem perda simultânea -> gargalo real no upload do Host!
        let host_bottleneck = vec![(v1, 110, 6.5), (v2, 125, 8.0), (v3, 25, 0.4)];
        let dec2 = evaluate_cluster_telemetry(&host_bottleneck).unwrap();
        assert!(dec2.host_upload_congested);
        assert!(dec2.global_loss_pct > 6.0);
    }

    #[tokio::test]
    async fn test_create_endpoint_and_ticket() -> Result<()> {
        let ep = create_endpoint().await?;
        let addr = ep.addr();
        let ticket = RoomTicket::new(&addr)?;
        assert!(ticket.raw.starts_with("lk-"));

        let recovered = ticket.to_endpoint_addr()?;
        assert_eq!(addr.id, recovered.id);
        let _ = ep.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_p2p_connection_control_and_datagrams() -> Result<()> {
        let host_ep = create_endpoint().await?;
        let host_addr = host_ep.addr();
        let ticket = RoomTicket::new(&host_addr)?;

        let viewer_ep = create_endpoint().await?;
        let target_addr = ticket.to_endpoint_addr()?;

        let (tx_done, rx_done) = tokio::sync::oneshot::channel::<()>();

        let host_handle = tokio::spawn(async move {
            let incoming = host_ep.accept().await.expect("host accept");
            let conn = incoming.await.expect("host conn");

            let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi");
            let msg = read_control(&mut recv).await.expect("read_control");
            match msg {
                Control::Watch { nick, .. } => {
                    assert_eq!(nick, "LucasViewer");
                    let accept_msg = Control::Accept {
                        video: VideoOffer { width: 1920, height: 1080, fps: 60, codec: "h264".into() },
                        audio: None,
                        udp_port: 0,
                        stream_token: None,
                    };
                    write_control(&mut send, &accept_msg).await.expect("write_control");
                }
                _ => panic!("esperava Watch"),
            }

            conn.send_datagram(Bytes::from_static(b"MEDIA_PACKET_FRAME_1")).expect("send_datagram");
            let _ = rx_done.await;
        });

        let viewer_handle = tokio::spawn(async move {
            let conn = viewer_ep.connect(target_addr, LUKE_SHARE_ALPN).await.expect("viewer connect");
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

        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(send);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(host_mgr.viewers_count(), 1);

        stop.store(true, Ordering::SeqCst);
        Ok(())
    }
}
