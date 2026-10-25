//! Orquestrador de recepção P2P (Receiver):
//! - Conecta ao transmissor via Iroh QUIC com Auto-Heal transparente de até 15s em quedas de rede/NAT.
//! - Remonta vídeo com FEC + NACK adaptativo ao RTT e despacha em IPC Binário puro (`VideoBin`, zero Base64).
//! - Reconstrói áudio com Audio RED + Opus In-Band FEC + PLC e reproduz nativamente via WASAPI no Rust.
//! - Suporta Cascateamento Mesh P2P autenticado (`LUKE_MESH_ALPN` + `verify_packet_tag`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::audio::opus_enc::OpusDec;
use crate::audio::player::NativeAudioPlayer;
use crate::net::p2p::{create_endpoint, RoomTicket, LUKE_MESH_ALPN, LUKE_SHARE_ALPN};
use crate::net::protocol::*;
use crate::net::short_code::resolve_short_code;
use crate::net::transport::{build_nack, Reassembler, RxEvent};
use crate::pipeline::StreamEvent;

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
                    let plc_count = gap.saturating_sub(2).min(2);
                    for _ in 0..plc_count {
                        if let Ok(pcm) = dec.decode_plc() {
                            out_frames.push(pcm);
                        }
                    }

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

struct EstablishedSession {
    conn: iroh::endpoint::Connection,
    send_ctrl: iroh::endpoint::SendStream,
    recv_ctrl: iroh::endpoint::RecvStream,
    video_offer: VideoOffer,
    stream_token: Option<[u8; 16]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionExitReason {
    UserStopped,
    HostEnded,
    NetworkDropped,
}

async fn establish_host_session(
    endpoint: &iroh::Endpoint,
    ticket: &RoomTicket,
    my_id: Uuid,
    my_nick: &str,
) -> Result<EstablishedSession> {
    let target_addr = ticket.to_endpoint_addr().context("ticket de sala inválido")?;
    let conn = endpoint
        .connect(target_addr, LUKE_SHARE_ALPN)
        .await
        .context("falha ao conectar ao anfitrião via Iroh P2P")?;

    let (mut send_ctrl, mut recv_ctrl) = conn
        .open_bi()
        .await
        .context("falha ao abrir stream de controle QUIC")?;

    let watch_msg = Control::Watch {
        peer_id: my_id,
        nick: my_nick.to_string(),
        udp_port: 0,
    };
    write_control(&mut send_ctrl, &watch_msg)
        .await
        .context("falha ao enviar Watch inicial")?;

    let resp = read_control(&mut recv_ctrl)
        .await
        .context("falha ao receber resposta do anfitrião")?;

    let (video_offer, stream_token) = match resp {
        Control::Accept {
            video, stream_token, ..
        } => (video, stream_token),
        Control::Reject { reason } => return Err(anyhow!("Conexão rejeitada pelo anfitrião: {reason}")),
        _ => return Err(anyhow!("Resposta inesperada do anfitrião")),
    };

    Ok(EstablishedSession {
        conn,
        send_ctrl,
        recv_ctrl,
        video_offer,
        stream_token,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_single_session(
    endpoint: &iroh::Endpoint,
    session: EstablishedSession,
    stop: &Arc<AtomicBool>,
    active_conn: &Arc<Mutex<Option<iroh::endpoint::Connection>>>,
    shared_send_ctrl: &Arc<tokio::sync::Mutex<Option<iroh::endpoint::SendStream>>>,
    video_tx: &crossbeam_channel::Sender<(bool, u32, Vec<u8>)>,
    audio_player: &Option<Arc<NativeAudioPlayer>>,
    downstream_mesh_peers: &Arc<Mutex<Vec<iroh::endpoint::Connection>>>,
    event_sink: &StreamEventSink,
) -> SessionExitReason {
    let EstablishedSession {
        conn,
        send_ctrl,
        mut recv_ctrl,
        video_offer,
        stream_token,
    } = session;

    *active_conn.lock() = Some(conn.clone());
    *shared_send_ctrl.lock().await = Some(send_ctrl);

    event_sink(StreamEvent::Config {
        width: video_offer.width,
        height: video_offer.height,
        fps: video_offer.fps,
        codec: video_offer.codec,
    });

    // Dispara HELLO + PLI imediatos e anuncia endereço Mesh ao Host
    let _ = conn.send_datagram(Bytes::from(control_packet(KIND_HELLO, 0, &[])));
    let _ = conn.send_datagram(Bytes::from(control_packet(KIND_PLI, 0, &[])));
    if let Ok(my_mesh_ticket) = RoomTicket::new(&endpoint.addr()) {
        let announce = control_packet(KIND_RELAY_ANNOUNCE, 0, my_mesh_ticket.raw.as_bytes());
        let _ = conn.send_datagram(Bytes::from(announce));
    }

    let session_alive = Arc::new(AtomicBool::new(true));
    let host_ended = Arc::new(AtomicBool::new(false));
    let relay_role_active = Arc::new(AtomicBool::new(false));
    let active_upstream_relay: Arc<Mutex<Option<(Uuid, [u8; 16])>>> = Arc::new(Mutex::new(None));
    let (mesh_dg_tx, mut mesh_dg_rx) = tokio::sync::mpsc::channel::<Bytes>(384);

    // Sub-tarefa de controle do Host (Reconfigure, AudioState, Viewers, RelayRole, RelayAssign, Bye)
    let st_global = stop.clone();
    let sess_alive_ctrl = session_alive.clone();
    let host_ended_ctrl = host_ended.clone();
    let sink_ctrl = event_sink.clone();
    let audio_ctrl = audio_player.clone();
    let relay_role_ctrl = relay_role_active.clone();
    let upstream_relay_ctrl = active_upstream_relay.clone();
    let ep_for_mesh = endpoint.clone();
    let send_ctrl_for_mesh = shared_send_ctrl.clone();
    let mesh_dg_tx_ctrl = mesh_dg_tx.clone();

    let ctrl_handle = tokio::spawn(async move {
        while !st_global.load(Ordering::SeqCst) && sess_alive_ctrl.load(Ordering::SeqCst) {
            match read_control(&mut recv_ctrl).await {
                Ok(Control::Reconfigure { video }) => {
                    sink_ctrl(StreamEvent::Config {
                        width: video.width,
                        height: video.height,
                        fps: video.fps,
                        codec: video.codec,
                    });
                }
                Ok(Control::AudioState { enabled }) => {
                    if let Some(ref ap) = audio_ctrl {
                        ap.set_muted(!enabled);
                    }
                    sink_ctrl(StreamEvent::AudioState { enabled });
                }
                Ok(Control::ViewersUpdate { count, viewers }) => {
                    sink_ctrl(StreamEvent::Viewers { count, viewers });
                }
                Ok(Control::RelayRole { active }) => {
                    relay_role_ctrl.store(active, Ordering::Relaxed);
                }
                Ok(Control::RelayAssign {
                    relay_ticket,
                    relay_peer_id,
                    stream_token: tok,
                }) => {
                    let ep_m = ep_for_mesh.clone();
                    let send_c = send_ctrl_for_mesh.clone();
                    let up_slot = upstream_relay_ctrl.clone();
                    let tx_m = mesh_dg_tx_ctrl.clone();
                    let alive_m = sess_alive_ctrl.clone();
                    let stop_m = st_global.clone();

                    tokio::spawn(async move {
                        let ticket = RoomTicket { raw: relay_ticket };
                        let Ok(target) = ticket.to_endpoint_addr() else { return };
                        let Ok(Ok(peer_conn)) = tokio::time::timeout(
                            Duration::from_secs(4),
                            ep_m.connect(target, LUKE_MESH_ALPN),
                        )
                        .await
                        else {
                            return;
                        };

                        *up_slot.lock() = Some((relay_peer_id, tok));
                        if let Some(ref mut sc) = *send_c.lock().await {
                            let _ = write_control(
                                sc,
                                &Control::RelayStatus {
                                    connected: true,
                                    relay_peer_id,
                                },
                            )
                            .await;
                        }
                        tracing::info!("receiver_p2p: cascata Mesh conectada ao par de fibra {relay_peer_id}!");

                        while !stop_m.load(Ordering::SeqCst) && alive_m.load(Ordering::SeqCst) {
                            match peer_conn.read_datagram().await {
                                Ok(dg) => {
                                    let _ = tx_m.send(dg).await;
                                }
                                Err(_) => break,
                            }
                        }

                        // Se o par retransmissor desconectar, avisa o Host imediatamente p/ retomar envio direto!
                        let should_notify = {
                            let mut g = up_slot.lock();
                            if g.map(|(id, _)| id) == Some(relay_peer_id) {
                                *g = None;
                                true
                            } else {
                                false
                            }
                        };
                        if should_notify {
                            if let Some(ref mut sc) = *send_c.lock().await {
                                let _ = write_control(
                                    sc,
                                    &Control::RelayStatus {
                                        connected: false,
                                        relay_peer_id,
                                    },
                                )
                                .await;
                            }
                        }
                    });
                }
                Ok(Control::Bye) => {
                    host_ended_ctrl.store(true, Ordering::SeqCst);
                    sess_alive_ctrl.store(false, Ordering::SeqCst);
                    break;
                }
                Err(_) => {
                    sess_alive_ctrl.store(false, Ordering::SeqCst);
                    break;
                }
                _ => {}
            }
        }
    });

    // Loop de datagramas de mídia (Host direto + Cascata Mesh autenticada)
    let mut reassembler = Reassembler::new();
    let mut audio_rx_stream = AudioStreamReceiver::new();
    reassembler.set_link_rtt_ms(active_quic_rtt_ms(&conn));

    let start_instant = Instant::now();
    let mut last_telem_instant = Instant::now();
    let mut last_relayed_rx = Instant::now();
    let mut tick_interval = tokio::time::interval(Duration::from_millis(4));
    let mut telem_interval = tokio::time::interval(Duration::from_millis(1000));
    let mut last_frames_delivered: u64 = 0;
    let mut last_pkts_rx: u64 = 0;
    let mut last_pkts_lost: u64 = 0;
    let session_tok = stream_token;

    while !stop.load(Ordering::SeqCst) && session_alive.load(Ordering::SeqCst) {
        tokio::select! {
            biased;
            res = conn.read_datagram() => {
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
                                                let _ = conn.send_datagram(Bytes::from(nack_pkt));
                                            }
                                            RxEvent::RequestKey => {
                                                let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                                                let _ = conn.send_datagram(Bytes::from(pli_pkt));
                                            }
                                        }
                                    }
                                    if relay_role_active.load(Ordering::Relaxed) {
                                        let mut peers = downstream_mesh_peers.lock();
                                        peers.retain(|p| p.send_datagram(dg.clone()).is_ok());
                                    }
                                }
                                KIND_AUDIO => {
                                    if let Some(ref ap) = audio_player {
                                        for pcm in audio_rx_stream.push_packet(&h, payload) {
                                            ap.push_samples(&pcm);
                                        }
                                    }
                                    if relay_role_active.load(Ordering::Relaxed) {
                                        let mut peers = downstream_mesh_peers.lock();
                                        peers.retain(|p| p.send_datagram(dg.clone()).is_ok());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_) => {
                        session_alive.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
            Some(mesh_dg) = mesh_dg_rx.recv() => {
                // Valida tag criptográfica do pacote antes de aceitar mídia vinda de outro viewer!
                let expected_tok = active_upstream_relay
                    .lock()
                    .map(|(_, t)| t)
                    .or(session_tok);
                if let Some(tok) = expected_tok {
                    if !verify_packet_tag(&mesh_dg, &tok) {
                        continue;
                    }
                }

                last_relayed_rx = Instant::now();
                let now_ms = start_instant.elapsed().as_millis() as u64;
                if let Some((h, payload)) = MediaHeader::parse(&mesh_dg) {
                    match h.kind {
                        KIND_VIDEO | KIND_FEC => {
                            let events = reassembler.push(&h, payload, now_ms);
                            for ev in events {
                                match ev {
                                    RxEvent::Frame { key, ts_90k, data, .. } => {
                                        let _ = video_tx.try_send((key, ts_90k, data));
                                    }
                                    RxEvent::Nack { frame_id, missing } => {
                                        // NACKs continuam indo direto ao Host (1 salto rápido no RetransmitRing!)
                                        let nack_pkt = build_nack(frame_id, &missing);
                                        let _ = conn.send_datagram(Bytes::from(nack_pkt));
                                    }
                                    RxEvent::RequestKey => {
                                        let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                                        let _ = conn.send_datagram(Bytes::from(pli_pkt));
                                    }
                                }
                            }
                        }
                        KIND_AUDIO => {
                            if let Some(ref ap) = audio_player {
                                for pcm in audio_rx_stream.push_packet(&h, payload) {
                                    ap.push_samples(&pcm);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ = tick_interval.tick() => {
                let quic_rtt = active_quic_rtt_ms(&conn);
                reassembler.set_link_rtt_ms(quic_rtt);

                // Watchdog da cascata Mesh: se o par upstream silenciar por > 800ms, volta ao Host direto na hora!
                let stalled_relay = {
                    let mut g = active_upstream_relay.lock();
                    if let Some((relay_id, _)) = *g {
                        if last_relayed_rx.elapsed() > Duration::from_millis(800) {
                            *g = None;
                            Some(relay_id)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                if let Some(relay_id) = stalled_relay {
                    if let Some(ref mut sc) = *shared_send_ctrl.lock().await {
                        let _ = write_control(sc, &Control::RelayStatus { connected: false, relay_peer_id: relay_id }).await;
                    }
                    let _ = conn.send_datagram(Bytes::from(control_packet(KIND_PLI, 0, &[])));
                }

                let now_ms = start_instant.elapsed().as_millis() as u64;
                let tick_events = reassembler.tick(now_ms);
                let mut saw_loss_spike = false;

                for ev in tick_events {
                    match ev {
                        RxEvent::Nack { frame_id, missing } => {
                            let nack_pkt = build_nack(frame_id, &missing);
                            let _ = conn.send_datagram(Bytes::from(nack_pkt));
                        }
                        RxEvent::RequestKey => {
                            saw_loss_spike = true;
                            let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                            let _ = conn.send_datagram(Bytes::from(pli_pkt));
                        }
                        _ => {}
                    }
                }

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
                        let _ = conn.send_datagram(Bytes::from(telem_pkt));
                    }
                }
            }
            _ = telem_interval.tick() => {
                let elapsed_sec = last_telem_instant.elapsed().as_secs_f32().max(0.1);
                last_telem_instant = Instant::now();

                let quic_rtt = active_quic_rtt_ms(&conn);
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

                let telem_report = TelemetryReport {
                    rtt_ms: stats.rtt_ms.max(1) as u16,
                    loss_pct_x10: ((loss_pct * 10.0) as u16).min(1000),
                    nacks_sent: (stats.nacks_sent as u16).min(u16::MAX),
                    fec_recovered: (stats.fec_recovered as u16).min(u16::MAX),
                };
                let telem_pkt = control_packet(KIND_TELEMETRY, 0, &telem_report.to_bytes());
                let _ = conn.send_datagram(Bytes::from(telem_pkt));

                let via_relay = active_upstream_relay.lock().is_some();
                event_sink(StreamEvent::Telemetry {
                    rtt_ms: stats.rtt_ms.max(1),
                    packets_rx: stats.packets_rx,
                    loss_pct,
                    nacks_recovered: stats.nacks_recovered,
                    fec_recovered: stats.fec_recovered,
                    dropped_ref: stats.dropped_ref_frames,
                    fps: effective_fps,
                    via_relay,
                });
            }
        }
    }

    ctrl_handle.abort();

    if stop.load(Ordering::SeqCst) {
        SessionExitReason::UserStopped
    } else if host_ended.load(Ordering::SeqCst) {
        SessionExitReason::HostEnded
    } else {
        SessionExitReason::NetworkDropped
    }
}

pub struct ReceiverPipeline {
    stop: Arc<AtomicBool>,
    p2p_endpoint: Arc<Mutex<Option<iroh::Endpoint>>>,
    active_conn: Arc<Mutex<Option<iroh::endpoint::Connection>>>,
    p2p_send_ctrl: Arc<tokio::sync::Mutex<Option<iroh::endpoint::SendStream>>>,
    audio_player: Option<Arc<NativeAudioPlayer>>,
    threads: Vec<JoinHandle<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ReceiverPipeline {
    /// Inicia recepção P2P Nativa sobre Iroh com áudio WASAPI nativo, IPC binário e Auto-Heal de 15s.
    pub async fn start_p2p(
        endpoint: iroh::Endpoint,
        ticket: RoomTicket,
        short_code_origin: Option<String>,
        my_id: Uuid,
        my_nick: String,
        enable_native_audio: bool,
        event_sink: StreamEventSink,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));

        // 1. Primeira conexão imediata (valida código/alcance na hora)
        let first_session = establish_host_session(&endpoint, &ticket, my_id, &my_nick).await?;
        tracing::info!("receiver_p2p: conexão P2P estabelecida com sucesso!");

        let audio_player = if enable_native_audio {
            Some(Arc::new(NativeAudioPlayer::start()))
        } else {
            None
        };

        let active_conn = Arc::new(Mutex::new(Some(first_session.conn.clone())));
        let p2p_send_ctrl = Arc::new(tokio::sync::Mutex::new(None));
        let p2p_endpoint = Arc::new(Mutex::new(Some(endpoint.clone())));

        // 2. Fila de quadros de vídeo despachados como IPC Binário (`VideoBin` -> ArrayBuffer zero-copy)
        let (video_tx, video_rx) = crossbeam_channel::bounded::<(bool, u32, Vec<u8>)>(96);
        let st_disp = stop.clone();
        let sink_disp = event_sink.clone();
        let dispatch_thread = std::thread::Builder::new()
            .name("receiver-bin-ipc-dispatch".into())
            .spawn(move || {
                while !st_disp.load(Ordering::SeqCst) {
                    match video_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok((key, pts, data)) => {
                            sink_disp(StreamEvent::VideoBin { key, pts, data });
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    }
                }
            })?;

        // 3. Escuta de pares downstream quando este espectador atua como Retransmissor Mesh (`LUKE_MESH_ALPN`)
        let downstream_mesh_peers: Arc<Mutex<Vec<iroh::endpoint::Connection>>> = Arc::new(Mutex::new(Vec::new()));
        let ep_slot_mesh = p2p_endpoint.clone();
        let mesh_peers_accept = downstream_mesh_peers.clone();
        let stop_mesh = stop.clone();
        let mesh_listener_task = tokio::spawn(async move {
            while !stop_mesh.load(Ordering::SeqCst) {
                let ep_opt = ep_slot_mesh.lock().clone();
                let Some(ep) = ep_opt else {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                };
                tokio::select! {
                    incoming = ep.accept() => {
                        let Some(incoming) = incoming else {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            continue;
                        };
                        if let Ok(peer_conn) = incoming.await {
                            if peer_conn.alpn() == LUKE_MESH_ALPN {
                                mesh_peers_accept.lock().push(peer_conn);
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                }
            }
        });

        // 4. Supervisor com Reconexão Automática Transparente (Auto-Heal por até 15s)
        let st_sup = stop.clone();
        let ep_slot_sup = p2p_endpoint.clone();
        let conn_slot_sup = active_conn.clone();
        let ctrl_slot_sup = p2p_send_ctrl.clone();
        let audio_sup = audio_player.clone();
        let mesh_sup = downstream_mesh_peers.clone();
        let sink_sup = event_sink.clone();

        let supervisor_task = tokio::spawn(async move {
            let mut cur_ep = endpoint;
            let mut cur_ticket = ticket;
            let mut cur_session = Some(first_session);
            let mut end_reason = "Transmissão encerrada".to_string();

            while !st_sup.load(Ordering::SeqCst) {
                let Some(sess) = cur_session.take() else { break };

                let exit = run_single_session(
                    &cur_ep,
                    sess,
                    &st_sup,
                    &conn_slot_sup,
                    &ctrl_slot_sup,
                    &video_tx,
                    &audio_sup,
                    &mesh_sup,
                    &sink_sup,
                )
                .await;

                match exit {
                    SessionExitReason::UserStopped => return,
                    SessionExitReason::HostEnded => {
                        end_reason = "O anfitrião encerrou a transmissão.".into();
                        break;
                    }
                    SessionExitReason::NetworkDropped => {
                        // Inicia janela de 15 segundos de Auto-Heal transparente!
                        tracing::warn!("receiver_p2p: queda abrupta de rede detectada — iniciando Auto-Heal (até 15s)...");
                        let heal_deadline = Instant::now() + Duration::from_secs(15);
                        let mut attempt: u32 = 0;
                        let mut reconnected = false;

                        while !st_sup.load(Ordering::SeqCst) && Instant::now() < heal_deadline {
                            attempt += 1;
                            let rem = heal_deadline
                                .saturating_duration_since(Instant::now())
                                .as_secs()
                                .max(1) as u32;
                            sink_sup(StreamEvent::Reconnecting {
                                attempt,
                                remaining_secs: rem,
                            });

                            let backoff = if attempt == 1 { 450 } else { 1050 };
                            tokio::time::sleep(Duration::from_millis(backoff)).await;
                            if st_sup.load(Ordering::SeqCst) {
                                return;
                            }

                            // Na 2ª+ tentativa, recria o socket UDP local p/ atravessar troca de porta NAT/Wi-Fi
                            if attempt >= 2 {
                                if let Ok(fresh_ep) = create_endpoint().await {
                                    let old = ep_slot_sup.lock().replace(fresh_ep.clone());
                                    if let Some(o) = old {
                                        tokio::spawn(async move {
                                            let _ = o.close().await;
                                        });
                                    }
                                    cur_ep = fresh_ep;
                                }
                                if let Some(ref sc) = short_code_origin {
                                    if let Ok(new_t) = resolve_short_code(sc).await {
                                        cur_ticket = new_t;
                                    }
                                }
                            }

                            match tokio::time::timeout(
                                Duration::from_millis(3600),
                                establish_host_session(&cur_ep, &cur_ticket, my_id, &my_nick),
                            )
                            .await
                            {
                                Ok(Ok(new_sess)) => {
                                    tracing::info!("receiver_p2p: Auto-Heal reconectado na tentativa #{attempt}!");
                                    sink_sup(StreamEvent::Reconnected);
                                    cur_session = Some(new_sess);
                                    reconnected = true;
                                    break;
                                }
                                _ => continue,
                            }
                        }

                        if !reconnected {
                            end_reason = "Conexão perdida após tentar reconectar automaticamente por 15s.".into();
                            break;
                        }
                    }
                }
            }

            if !st_sup.load(Ordering::SeqCst) {
                st_sup.store(true, Ordering::SeqCst);
                sink_sup(StreamEvent::Ended { reason: end_reason });
            }
        });

        Ok(Self {
            stop,
            p2p_endpoint,
            active_conn,
            p2p_send_ctrl,
            audio_player,
            threads: vec![dispatch_thread],
            tasks: vec![supervisor_task, mesh_listener_task],
        })
    }

    pub fn request_keyframe(&self) {
        let pli_pkt = control_packet(KIND_PLI, 0, &[]);
        if let Some(conn) = self.active_conn.lock().as_ref() {
            let _ = conn.send_datagram(Bytes::from(pli_pkt));
        }
    }

    pub fn set_volume(&self, volume: f32) {
        if let Some(ref ap) = self.audio_player {
            ap.set_volume(volume);
        }
    }

    pub fn set_muted(&self, muted: bool) {
        if let Some(ref ap) = self.audio_player {
            ap.set_muted(muted);
        }
    }

    pub async fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut sc) = self.p2p_send_ctrl.lock().await.take() {
            let _ = write_control(&mut sc, &Control::Bye).await;
        }
        if let Some(conn) = self.active_conn.lock().take() {
            conn.close(0u32.into(), b"bye");
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let ep_opt = self.p2p_endpoint.lock().take();
        if let Some(ep) = ep_opt {
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
    async fn test_receiver_pipeline_p2p_binary_ipc_and_auto_heal() -> Result<()> {
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
            VideoOffer {
                width: 1920,
                height: 1080,
                fps: 60,
                codec: "h264".into(),
            },
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
            None,
            Uuid::new_v4(),
            "ViewerTest".into(),
            false,
            event_sink,
        )
        .await?;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(host_mgr.viewers_count(), 1);
        assert!(pli_called.load(Ordering::SeqCst));

        // 1. Testa entrega binária VideoBin (sem Base64!)
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
            if let Ok(StreamEvent::VideoBin { key, pts, data }) = ev_rx.try_recv() {
                assert!(key);
                assert_eq!(pts, 9000);
                assert_eq!(data.len(), 2500);
                got_video = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(got_video, "Receptor deve entregar quadro binário puro via VideoBin!");

        // 2. Simula queda abrupta da conexão QUIC no lado do Host (sem enviar Control::Bye)
        for v in host_mgr.active_viewers() {
            v.conn.close(1u32.into(), b"simulated_nat_drop");
        }

        // O Receptor deve emitir Reconnecting -> Reconnected automaticamente em < 3s!
        let mut saw_reconnecting = false;
        let mut saw_reconnected = false;
        let heal_wait = Instant::now() + Duration::from_secs(6);
        while Instant::now() < heal_wait {
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    StreamEvent::Reconnecting { .. } => saw_reconnecting = true,
                    StreamEvent::Reconnected => {
                        saw_reconnected = true;
                        break;
                    }
                    _ => {}
                }
            }
            if saw_reconnected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert!(saw_reconnecting, "Auto-Heal deve notificar estado Reconnecting");
        assert!(saw_reconnected, "Auto-Heal deve reconectar transparentemente ao Host!");

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

        let mut total_pcm_chunks = 0;
        for seq in [1usize, 3, 5, 7, 9, 10] {
            let (h, pl) = MediaHeader::parse(&wire_pkts[seq - 1]).unwrap();
            total_pcm_chunks += rx.push_packet(&h, pl).len();
        }
        assert_eq!(total_pcm_chunks, 10);

        let (h13, pl13) = MediaHeader::parse(&wire_pkts[12]).unwrap();
        let recovered_burst = rx.push_packet(&h13, pl13);
        assert_eq!(recovered_burst.len(), 3);

        let (h15, pl15) = MediaHeader::parse(&wire_pkts[14]).unwrap();
        assert_eq!(rx.push_packet(&h15, pl15).len(), 2);
        let (h14, pl14) = MediaHeader::parse(&wire_pkts[13]).unwrap();
        assert_eq!(rx.push_packet(&h14, pl14).len(), 0);
    }
}
