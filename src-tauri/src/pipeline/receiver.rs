//! Orquestrador de recepção P2P (Receiver): conecta ao transmissor via Iroh QUIC,
//! recebe datagramas de mídia em alta velocidade, remonta vídeo com FEC + NACK adaptativo ao RTT,
//! reconstrói perdas de áudio via Audio RED + Opus In-Band FEC + PLC e despacha para o WebView2.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use uuid::Uuid;

use crate::audio::opus_enc::OpusDec;
use crate::net::p2p::{RoomTicket, LUKE_SHARE_ALPN};
use crate::net::protocol::*;
use crate::net::transport::{build_nack, Reassembler, RxEvent};
use crate::pipeline::{base64_encode, StreamEvent};

pub type StreamEventSink = Arc<dyn Fn(StreamEvent) + Send + Sync>;

fn active_quic_rtt_ms(conn: &iroh::endpoint::Connection) -> u32 {
    let paths = conn.paths();
    paths
        .iter()
        .find(|p| p.is_selected())
        .or_else(|| paths.iter().next())
        .map(|p| p.rtt().as_millis() as u32)
        .unwrap_or(0)
}

/// Deduplicador de pacotes de áudio baseado no frame_id sequencial.
/// Garante que mesmo com múltiplos caminhos de rede (Mesh Relay, RED ou pacotes atrasados)
/// cada quadro de áudio PCM só seja decodificado e reproduzido uma única vez.
pub struct AudioDeduplicator {
    seen: [u32; 256],
    idx: usize,
}

impl AudioDeduplicator {
    pub fn new() -> Self {
        Self {
            seen: [0; 256],
            idx: 0,
        }
    }

    pub fn contains(&self, fid: u32) -> bool {
        fid != 0 && self.seen.contains(&fid)
    }

    pub fn mark(&mut self, fid: u32) {
        if fid == 0 || self.seen.contains(&fid) {
            return;
        }
        self.seen[self.idx] = fid;
        self.idx = (self.idx + 1) % self.seen.len();
    }

    /// Retorna `true` se o `fid` já foi processado recentemente (ou seja, é duplicata).
    pub fn is_duplicate(&mut self, fid: u32) -> bool {
        if fid == 0 {
            return false;
        }
        if self.seen.contains(&fid) {
            return true;
        }
        self.mark(fid);
        false
    }
}

/// Receptor de áudio resiliente contra perda de pacotes:
/// 1. Recupera 1 pacote perdido de forma 100% lossless via Audio RED (`prev_opus`).
/// 2. Recupera 2 pacotes consecutivos via Opus In-Band FEC (`decode_fec`) + Audio RED.
/// 3. Suaviza rajadas de 3..5 pacotes perdidos usando Opus PLC (`decode_plc`) + FEC + RED.
pub struct AudioStreamReceiver {
    dec: Option<OpusDec>,
    dedup: AudioDeduplicator,
    last_fid: Option<u32>,
}

impl AudioStreamReceiver {
    pub fn new() -> Self {
        let dec = match OpusDec::new() {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!("receiver_p2p: falha ao inicializar OpusDec: {e:#}");
                None
            }
        };
        Self {
            dec,
            dedup: AudioDeduplicator::new(),
            last_fid: None,
        }
    }

    pub fn push_packet(&mut self, h: &MediaHeader, payload: &[u8]) -> Vec<Vec<f32>> {
        let mut out_frames = Vec::new();
        let Some(dec) = &mut self.dec else {
            return out_frames;
        };

        let fid = h.frame_id;
        if let Some(last) = self.last_fid {
            if (fid.wrapping_sub(last) as i32) <= 0 {
                return out_frames;
            }
        }
        if self.dedup.contains(fid) {
            return out_frames;
        }

        let (prev_opus, curr_opus) = unpack_audio_red(h.flags, payload);

        if let Some(last) = self.last_fid {
            let diff = fid.wrapping_sub(last) as i32;
            if diff > 1 {
                let gap = (diff - 1) as usize;
                let prev_fid = fid.wrapping_sub(1);

                if gap == 1 {
                    // 1 pacote perdido (fid - 1): recupera bit-a-bit via RED ou via Opus In-Band FEC
                    if !self.dedup.contains(prev_fid) {
                        if let Some(prev_bytes) = prev_opus {
                            if let Ok(pcm) = dec.decode(prev_bytes) {
                                self.dedup.mark(prev_fid);
                                out_frames.push(pcm);
                            }
                        } else if let Ok(pcm) = dec.decode_fec(curr_opus) {
                            self.dedup.mark(prev_fid);
                            out_frames.push(pcm);
                        }
                    }
                } else if gap <= 5 {
                    // Rajada curta de perda (2 a 5 pacotes = 40ms a 100ms):
                    // Usa PLC para pacotes mais antigos, FEC no penúltimo e RED no último perdido
                    let plc_count = gap.saturating_sub(2).min(2);
                    for _ in 0..plc_count {
                        if let Ok(pcm) = dec.decode_plc() {
                            out_frames.push(pcm);
                        }
                    }

                    // Quadro fid - 2 reconstruído via In-Band FEC contido em prev_opus (se disponível) ou PLC
                    let fid_minus_2 = fid.wrapping_sub(2);
                    if !self.dedup.contains(fid_minus_2) {
                        let recovered = if let Some(prev_bytes) = prev_opus {
                            dec.decode_fec(prev_bytes).or_else(|_| dec.decode_plc())
                        } else {
                            dec.decode_plc()
                        };
                        if let Ok(pcm) = recovered {
                            self.dedup.mark(fid_minus_2);
                            out_frames.push(pcm);
                        }
                    }

                    // Quadro fid - 1 reconstruído bit-a-bit via Audio RED ou In-Band FEC de curr_opus
                    if !self.dedup.contains(prev_fid) {
                        let recovered = if let Some(prev_bytes) = prev_opus {
                            dec.decode(prev_bytes)
                        } else {
                            dec.decode_fec(curr_opus)
                        };
                        if let Ok(pcm) = recovered {
                            self.dedup.mark(prev_fid);
                            out_frames.push(pcm);
                        }
                    }
                }
            }
        }

        if let Ok(pcm) = dec.decode(curr_opus) {
            self.dedup.mark(fid);
            self.last_fid = Some(fid);
            out_frames.push(pcm);
        }

        out_frames
    }
}

pub struct ReceiverPipeline {
    stop: Arc<AtomicBool>,
    p2p_endpoint: Option<iroh::Endpoint>,
    p2p_conn: Option<iroh::endpoint::Connection>,
    p2p_send_ctrl: Arc<tokio::sync::Mutex<Option<iroh::endpoint::SendStream>>>,
    threads: Vec<JoinHandle<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ReceiverPipeline {
    /// Conexão P2P Nativa sobre Iroh (Internet direta via RoomTicket).
    pub async fn start_p2p(
        endpoint: iroh::Endpoint,
        ticket: RoomTicket,
        my_id: Uuid,
        my_nick: String,
        event_sink: StreamEventSink,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));

        // 1. Resolve o endereço do Endpoint a partir do Ticket de convite
        let target_addr = ticket.to_endpoint_addr().context("ticket de sala inválido")?;
        tracing::info!("receiver_p2p: conectando ao host P2P {}...", target_addr.id);

        // 2. Conecta ao host via Iroh (QUIC + TLS 1.3 + STUN / Relay)
        let conn = endpoint
            .connect(target_addr, LUKE_SHARE_ALPN)
            .await
            .context("falha ao conectar ao anfitrião via Iroh P2P")?;
        tracing::info!("receiver_p2p: conexão P2P estabelecida com sucesso!");

        // 3. Abre stream bi-direcional de controle
        let (mut send_ctrl, mut recv_ctrl) = conn
            .open_bi()
            .await
            .context("falha ao abrir stream de controle QUIC")?;

        let watch_msg = Control::Watch {
            peer_id: my_id,
            nick: my_nick,
            udp_port: 0,
        };
        write_control(&mut send_ctrl, &watch_msg)
            .await
            .context("falha ao enviar Watch inicial")?;

        let resp = read_control(&mut recv_ctrl)
            .await
            .context("falha ao receber resposta do anfitrião")?;

        let initial_video = match resp {
            Control::Accept { video, .. } => video,
            Control::Reject { reason } => return Err(anyhow!("Conexão rejeitada pelo anfitrião: {reason}")),
            _ => return Err(anyhow!("Resposta inesperada do anfitrião")),
        };

        // Notifica o frontend com a resolução inicial
        event_sink(StreamEvent::Config {
            width: initial_video.width,
            height: initial_video.height,
            fps: initial_video.fps,
            codec: initial_video.codec.clone(),
        });

        // Mantém a stream de envio de controle viva durante toda a sessão
        let p2p_send_ctrl = Arc::new(tokio::sync::Mutex::new(Some(send_ctrl)));

        // Envia datagramas HELLO e PLI imediatos para abrir rota e solicitar Keyframe IDR de entrada
        let hello_pkt = control_packet(KIND_HELLO, 0, &[]);
        let _ = conn.send_datagram(Bytes::from(hello_pkt));
        let pli_pkt = control_packet(KIND_PLI, 0, &[]);
        let _ = conn.send_datagram(Bytes::from(pli_pkt));

        // Anuncia ticket local ao host para permitir Relé Mesh assistido entre espectadores
        if let Ok(my_ticket) = RoomTicket::new(&endpoint.addr()) {
            let announce_pkt = control_packet(KIND_RELAY_ANNOUNCE, 0, my_ticket.raw.as_bytes());
            let _ = conn.send_datagram(Bytes::from(announce_pkt));
        }

        // 4. Filas desacopladas para vídeo e áudio (ampliadas para absorver rajadas de recuperação NACK)
        let (video_tx, video_rx) = crossbeam_channel::bounded::<(bool, u32, Vec<u8>)>(96);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<u8>>(96);

        // --- Thread A: Despacho IPC para o frontend (Base64 + Channel.send) ---
        let st_disp = stop.clone();
        let sink_disp = event_sink.clone();
        let dispatch_thread = std::thread::Builder::new()
            .name("receiver-ipc-dispatch".into())
            .spawn(move || {
                while !st_disp.load(Ordering::SeqCst) {
                    crossbeam_channel::select! {
                        recv(video_rx) -> msg => match msg {
                            Ok((key, pts, data)) => {
                                let b64 = base64_encode(&data);
                                sink_disp(StreamEvent::Video {
                                    key,
                                    pts,
                                    data: b64,
                                });
                            }
                            Err(_) => break,
                        },
                        recv(audio_rx) -> msg => match msg {
                            Ok(bytes) => {
                                let b64 = base64_encode(&bytes);
                                sink_disp(StreamEvent::Audio {
                                    data: b64,
                                });
                            }
                            Err(_) => break,
                        },
                        default(Duration::from_millis(50)) => {}
                    }
                }

                sink_disp(StreamEvent::Ended {
                    reason: "Transmissão encerrada".into(),
                });
            })?;

        // --- Tarefa C: Escuta de mensagens de controle do anfitrião em segundo plano ---
        let st_ctrl = stop.clone();
        let sink_ctrl_reconfig = event_sink.clone();
        let sink_ctrl_audio = event_sink.clone();
        let sink_ctrl_viewers = event_sink.clone();
        let ctrl_task = tokio::spawn(async move {
            while !st_ctrl.load(Ordering::SeqCst) {
                match read_control(&mut recv_ctrl).await {
                    Ok(Control::Reconfigure { video }) => {
                        sink_ctrl_reconfig(StreamEvent::Config {
                            width: video.width,
                            height: video.height,
                            fps: video.fps,
                            codec: video.codec.clone(),
                        });
                    }
                    Ok(Control::AudioState { enabled }) => {
                        sink_ctrl_audio(StreamEvent::AudioState { enabled });
                    }
                    Ok(Control::ViewersUpdate { count, viewers }) => {
                        sink_ctrl_viewers(StreamEvent::Viewers { count, viewers });
                    }
                    Ok(Control::Bye) => {
                        st_ctrl.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(_) => {
                        st_ctrl.store(true, Ordering::SeqCst);
                        break;
                    }
                    _ => {}
                }
            }
        });

        // --- Tarefa D: Escuta e gerenciamento de conexões Mesh Relay entre espectadores ---
        let mesh_peers: Arc<parking_lot::Mutex<Vec<iroh::endpoint::Connection>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (mesh_dg_tx, mut mesh_dg_rx) = tokio::sync::mpsc::channel::<Bytes>(256);

        let ep_mesh_listener = endpoint.clone();
        let mesh_peers_listener = mesh_peers.clone();
        let stop_mesh = stop.clone();
        let mesh_dg_tx_accept = mesh_dg_tx.clone();
        let mesh_listener_task = tokio::spawn(async move {
            while !stop_mesh.load(Ordering::SeqCst) {
                tokio::select! {
                    incoming = ep_mesh_listener.accept() => {
                        let Some(incoming) = incoming else { break };
                        if let Ok(peer_conn) = incoming.await {
                            mesh_peers_listener.lock().push(peer_conn.clone());
                            let tx_in = mesh_dg_tx_accept.clone();
                            let st_p = stop_mesh.clone();
                            tokio::spawn(async move {
                                while !st_p.load(Ordering::SeqCst) {
                                    match peer_conn.read_datagram().await {
                                        Ok(dg) => {
                                            let _ = tx_in.send(dg).await;
                                        }
                                        Err(_) => break,
                                    }
                                }
                            });
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        });

        // --- Tarefa B: Loop de recepção e remontagem de Datagramas QUIC com Telemetria Ágil (1s) ---
        let st_p2p = stop.clone();
        let sink_telem = event_sink.clone();
        let conn_rx = conn.clone();
        let ep_mesh_conn = endpoint.clone();
        let mesh_peers_fwd = mesh_peers.clone();
        let mesh_dg_tx_mesh = mesh_dg_tx.clone();
        let p2p_task = tokio::spawn(async move {
            let mut reassembler = Reassembler::new();
            let mut audio_rx_stream = AudioStreamReceiver::new();

            let initial_rtt = active_quic_rtt_ms(&conn_rx);
            reassembler.set_link_rtt_ms(initial_rtt);

            let start_instant = Instant::now();
            let mut last_telem_instant = Instant::now();
            let mut tick_interval = tokio::time::interval(Duration::from_millis(4));
            let mut telem_interval = tokio::time::interval(Duration::from_millis(1000));
            let mut last_frames_delivered: u64 = 0;
            let mut last_pkts_rx: u64 = 0;
            let mut last_pkts_lost: u64 = 0;

            while !st_p2p.load(Ordering::SeqCst) {
                tokio::select! {
                    biased;
                    res = conn_rx.read_datagram() => {
                        match res {
                            Ok(dg) => {
                                let now_ms = start_instant.elapsed().as_millis() as u64;
                                if let Some((h, payload)) = MediaHeader::parse(&dg) {
                                    match h.kind {
                                        KIND_VIDEO | KIND_FEC => {
                                            let events = reassembler.push(&h, payload, now_ms);
                                            for ev in events {
                                                match ev {
                                                    RxEvent::Frame { key, ts_90k, data, .. } => {
                                                        let _ = video_tx.try_send((key, ts_90k, data));
                                                    }
                                                    RxEvent::Nack { frame_id, missing } => {
                                                        let nack_pkt = build_nack(frame_id, &missing);
                                                        let _ = conn_rx.send_datagram(Bytes::from(nack_pkt));
                                                    }
                                                    RxEvent::RequestKey => {
                                                        let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                                                        let _ = conn_rx.send_datagram(Bytes::from(pli_pkt));
                                                    }
                                                }
                                            }
                                            // Relé Mesh Assistido: retransmite vídeo aos peers mesh conectados
                                            let peers = mesh_peers_fwd.lock().clone();
                                            for p in peers {
                                                let _ = p.send_datagram(dg.clone());
                                            }
                                        }
                                        KIND_AUDIO => {
                                            for pcm in audio_rx_stream.push_packet(&h, payload) {
                                                let bytes: &[u8] = unsafe {
                                                    std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 4)
                                                };
                                                let _ = audio_tx.try_send(bytes.to_vec());
                                            }
                                        }
                                        KIND_RELAY_ANNOUNCE => {
                                            if let Ok(ticket_str) = std::str::from_utf8(payload) {
                                                if ticket_str.starts_with("lk-") {
                                                    let ep_m = ep_mesh_conn.clone();
                                                    let peers_m = mesh_peers_fwd.clone();
                                                    let tx_m = mesh_dg_tx_mesh.clone();
                                                    let st_m = st_p2p.clone();
                                                    let t_str = ticket_str.to_string();
                                                    tokio::spawn(async move {
                                                        let ticket = RoomTicket { raw: t_str };
                                                        if let Ok(target) = ticket.to_endpoint_addr() {
                                                            if let Ok(peer_conn) = ep_m.connect(target, LUKE_SHARE_ALPN).await {
                                                                tracing::info!("receiver_p2p: conectado ao peer mesh assistido!");
                                                                peers_m.lock().push(peer_conn.clone());
                                                                while !st_m.load(Ordering::SeqCst) {
                                                                    match peer_conn.read_datagram().await {
                                                                        Ok(dg) => {
                                                                            let _ = tx_m.send(dg).await;
                                                                        }
                                                                        Err(_) => break,
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    });
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Err(_) => {
                                tracing::info!("receiver_p2p: conexão de datagramas finalizada");
                                break;
                            }
                        }
                    }
                    Some(mesh_dg) = mesh_dg_rx.recv() => {
                        let now_ms = start_instant.elapsed().as_millis() as u64;
                        if let Some((h, payload)) = MediaHeader::parse(&mesh_dg) {
                            match h.kind {
                                KIND_VIDEO | KIND_FEC => {
                                    let events = reassembler.push(&h, payload, now_ms);
                                    for ev in events {
                                        if let RxEvent::Frame { key, ts_90k, data, .. } = ev {
                                            let _ = video_tx.try_send((key, ts_90k, data));
                                        }
                                    }
                                }
                                KIND_AUDIO => {
                                    for pcm in audio_rx_stream.push_packet(&h, payload) {
                                        let bytes: &[u8] = unsafe {
                                            std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 4)
                                        };
                                        let _ = audio_tx.try_send(bytes.to_vec());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ = tick_interval.tick() => {
                        let quic_rtt = active_quic_rtt_ms(&conn_rx);
                        reassembler.set_link_rtt_ms(quic_rtt);

                        let now_ms = start_instant.elapsed().as_millis() as u64;
                        let tick_events = reassembler.tick(now_ms);
                        let mut saw_loss_spike = false;

                        for ev in tick_events {
                            match ev {
                                RxEvent::Nack { frame_id, missing } => {
                                    let nack_pkt = build_nack(frame_id, &missing);
                                    let _ = conn_rx.send_datagram(Bytes::from(nack_pkt));
                                }
                                RxEvent::RequestKey => {
                                    saw_loss_spike = true;
                                    let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                                    let _ = conn_rx.send_datagram(Bytes::from(pli_pkt));
                                }
                                _ => {}
                            }
                        }

                        // Reporte rápido de congestionamento se houve queda severa há mais de 350ms
                        if saw_loss_spike && last_telem_instant.elapsed() >= Duration::from_millis(350) {
                            let stats = reassembler.stats();
                            let pkts_delta = stats.packets_rx.saturating_sub(last_pkts_rx);
                            let lost_delta = stats.packets_lost.saturating_sub(last_pkts_lost);
                            let total_pkts = pkts_delta + lost_delta;
                            if lost_delta > 0 && total_pkts > 0 {
                                let loss_pct = (lost_delta as f32 / total_pkts as f32) * 100.0;
                                let telem_report = TelemetryReport {
                                    rtt_ms: stats.rtt_ms.max(1) as u16,
                                    loss_pct_x10: ((loss_pct * 10.0) as u16).clamp(30, 1000),
                                    nacks_sent: (stats.nacks_sent as u16).min(u16::MAX),
                                    fec_recovered: (stats.fec_recovered as u16).min(u16::MAX),
                                };
                                let telem_pkt = control_packet(KIND_TELEMETRY, 0, &telem_report.to_bytes());
                                let _ = conn_rx.send_datagram(Bytes::from(telem_pkt));
                            }
                        }
                    }
                    _ = telem_interval.tick() => {
                        let elapsed_sec = last_telem_instant.elapsed().as_secs_f32().max(0.1);
                        last_telem_instant = Instant::now();

                        let quic_rtt = active_quic_rtt_ms(&conn_rx);
                        reassembler.set_link_rtt_ms(quic_rtt);

                        let stats = reassembler.stats();
                        let frames_delta = stats.frames_delivered.saturating_sub(last_frames_delivered);
                        last_frames_delivered = stats.frames_delivered;
                        let effective_fps = (frames_delta as f32 / elapsed_sec).round() as u32;

                        let pkts_delta = stats.packets_rx.saturating_sub(last_pkts_rx);
                        let lost_delta = stats.packets_lost.saturating_sub(last_pkts_lost);
                        last_pkts_rx = stats.packets_rx;
                        last_pkts_lost = stats.packets_lost;

                        let total_pkts = pkts_delta + lost_delta;
                        let loss_pct = if total_pkts > 0 {
                            (lost_delta as f32 / total_pkts as f32) * 100.0
                        } else {
                            0.0
                        };

                        // Envia relatório de telemetria de volta para o host (ajuste AIMD de bitrate e FEC)
                        let telem_report = TelemetryReport {
                            rtt_ms: stats.rtt_ms.max(1) as u16,
                            loss_pct_x10: ((loss_pct * 10.0) as u16).min(1000),
                            nacks_sent: (stats.nacks_sent as u16).min(u16::MAX),
                            fec_recovered: (stats.fec_recovered as u16).min(u16::MAX),
                        };
                        let telem_pkt = control_packet(KIND_TELEMETRY, 0, &telem_report.to_bytes());
                        let _ = conn_rx.send_datagram(Bytes::from(telem_pkt));

                        sink_telem(StreamEvent::Telemetry {
                            rtt_ms: stats.rtt_ms.max(1),
                            packets_rx: stats.packets_rx,
                            loss_pct,
                            nacks_recovered: stats.nacks_recovered,
                            fec_recovered: stats.fec_recovered,
                            dropped_ref: stats.dropped_ref_frames,
                            fps: effective_fps,
                        });
                    }
                }
            }
        });

        Ok(Self {
            stop,
            p2p_endpoint: Some(endpoint),
            p2p_conn: Some(conn),
            p2p_send_ctrl,
            threads: vec![dispatch_thread],
            tasks: vec![ctrl_task, p2p_task, mesh_listener_task],
        })
    }

    pub fn request_keyframe(&self) {
        let pli_pkt = control_packet(KIND_PLI, 0, &[]);
        if let Some(conn) = &self.p2p_conn {
            let _ = conn.send_datagram(Bytes::from(pli_pkt));
        }
    }

    pub async fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut sc) = self.p2p_send_ctrl.lock().await.take() {
            let _ = write_control(&mut sc, &Control::Bye).await;
        }
        if let Some(conn) = &self.p2p_conn {
            conn.close(0u32.into(), b"bye");
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(ep) = self.p2p_endpoint.take() {
            let _ = ep.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::opus_enc::OpusEnc;
    use crate::net::p2p::{create_endpoint, P2pHostManager, RoomTicket};
    use crate::net::transport::{packetize, RetransmitRing};
    use crossbeam_channel::unbounded;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_receiver_pipeline_p2p_e2e() -> Result<()> {
        let host_ep = create_endpoint().await?;
        let host_addr = host_ep.addr();
        let ticket = RoomTicket::new(&host_addr)?;
        let host_mgr = Arc::new(P2pHostManager::new());
        let ring = Arc::new(parking_lot::Mutex::new(RetransmitRing::new(100)));
        let stop = Arc::new(AtomicBool::new(false));
        let nacks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pli_called = Arc::new(AtomicBool::new(false));
        let pli_flag = pli_called.clone();

        host_mgr.clone().start_listener(
            host_ep,
            VideoOffer { width: 1920, height: 1080, fps: 60, codec: "h264".into() },
            None,
            ring.clone(),
            Arc::new(move || {
                pli_flag.store(true, Ordering::SeqCst);
            }),
            nacks,
            Arc::new(|_| {}),
            stop.clone(),
        );

        let viewer_ep = create_endpoint().await?;
        let (ev_tx, ev_rx) = unbounded::<StreamEvent>();
        let event_sink = Arc::new(move |ev: StreamEvent| {
            let _ = ev_tx.send(ev);
        });

        let mut receiver = ReceiverPipeline::start_p2p(
            viewer_ep,
            ticket,
            Uuid::new_v4(),
            "ViewerTest".into(),
            event_sink,
        ).await?;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(host_mgr.viewers_count(), 1, "Host deve ter 1 espectador ativo");
        assert!(pli_called.load(Ordering::SeqCst), "Host deve ter recebido pedido de Keyframe inicial");

        let frame_data = vec![0x12; 2500];
        let pkts = packetize(KIND_VIDEO, 1, true, 9000, &frame_data);
        let viewers = host_mgr.active_viewers();
        for p in pkts {
            let b = Bytes::copy_from_slice(&p);
            for v in &viewers {
                let _ = v.send_datagram(b.clone());
            }
        }

        let mut got_video = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(ev) = ev_rx.try_recv() {
                if let StreamEvent::Video { key, pts, .. } = ev {
                    assert!(key);
                    assert_eq!(pts, 9000);
                    got_video = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(got_video, "Receptor deve receber o frame remontado e despachar via IPC!");

        receiver.stop().await;
        stop.store(true, Ordering::SeqCst);
        Ok(())
    }

    #[test]
    fn test_audio_deduplicator() {
        let mut dedup = AudioDeduplicator::new();
        assert!(!dedup.is_duplicate(1));
        assert!(dedup.is_duplicate(1));
        assert!(!dedup.is_duplicate(2));
        assert!(dedup.is_duplicate(2));
        assert!(dedup.is_duplicate(1));

        for fid in 100..356 {
            assert!(!dedup.is_duplicate(fid));
        }
        assert!(!dedup.is_duplicate(1));
        assert!(dedup.is_duplicate(1));
    }

    #[test]
    fn test_audio_red_and_plc_resilience_under_heavy_loss() {
        let mut enc = OpusEnc::new(128_000).unwrap();
        let mut rx = AudioStreamReceiver::new();

        // Gera 20 pacotes de áudio RED reais de 20ms
        let mut wire_pkts = Vec::new();
        let mut prev_opus: Option<Vec<u8>> = None;
        for seq in 1..=20u32 {
            let pcm: Vec<f32> = (0..960)
                .flat_map(|i| {
                    let t = ((seq as usize - 1) * 960 + i) as f32 / 48000.0;
                    let v = 0.4 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                    [v, v]
                })
                .collect();
            let cur = enc.encode(&pcm).unwrap().to_vec();
            let pkt = pack_audio_red(prev_opus.as_deref(), &cur, seq, seq * 1800);
            prev_opus = Some(cur);
            wire_pkts.push(pkt);
        }

        // Cenário 1: 50% de perda alternada nos pacotes 1..=10 (perde 2, 4, 6, 8)
        let mut total_pcm_chunks = 0;
        for seq in [1usize, 3, 5, 7, 9, 10] {
            let (h, pl) = MediaHeader::parse(&wire_pkts[seq - 1]).unwrap();
            total_pcm_chunks += rx.push_packet(&h, pl).len();
        }
        // Todos os 10 quadros de 20ms foram reconstruídos sem buracos!
        assert_eq!(total_pcm_chunks, 10);

        // Cenário 2: Perda de 2 pacotes seguidos (perde 11 e 12; chega 13)
        let (h13, pl13) = MediaHeader::parse(&wire_pkts[12]).unwrap();
        let recovered_burst = rx.push_packet(&h13, pl13);
        assert_eq!(recovered_burst.len(), 3, "Deve entregar FEC(#11) + RED(#12) + Primário(#13)");

        // Cenário 3: Chegada fora de ordem (#15 chega antes do #14)
        let (h15, pl15) = MediaHeader::parse(&wire_pkts[14]).unwrap();
        assert_eq!(rx.push_packet(&h15, pl15).len(), 2, "Entrega #14 via RED + #15");
        let (h14, pl14) = MediaHeader::parse(&wire_pkts[13]).unwrap();
        assert_eq!(rx.push_packet(&h14, pl14).len(), 0, "Pacote atrasado #14 ignorado sem duplicar");
    }
}
