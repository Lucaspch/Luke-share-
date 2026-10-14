//! Orquestrador de transmissão (Sender): captura WGC, scaler, encoder H.264,
//! captura de áudio WASAPI com filtro do Discord, codificação Opus e transporte UDP com retransmissão NACK.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use parking_lot::Mutex;

use crate::audio::opus_enc::OpusEnc;
use crate::audio::{AudioCapturer, AudioTarget};
use crate::capture::sources::SourceInfo;
use crate::capture::wgc::WgcCapture;
use crate::net::p2p::{P2pHostManager, RoomTicket};
use crate::net::protocol::*;
use crate::net::session::ControlServer;
use crate::net::transport::{
    generate_fec, packetize, parse_nack_payload, CongestionController, RetransmitRing,
    DEFAULT_FEC_BLOCK_SIZE,
};
use crate::video::d3d::Gpu;
use crate::video::encoder::{EncodedFrame, EncoderConfig, H264Encoder};
use crate::video::resolution::{calculate_adaptive_bitrate, target_size, Preset};
use crate::video::scaler::Scaler;

pub type PreviewSink = Arc<dyn Fn(bool, u32, Vec<u8>) + Send + Sync>;

pub struct SenderPipeline {
    stop: Arc<AtomicBool>,
    encoder: Arc<H264Encoder>,
    audio_cap: AudioCapturer,
    control_server: Arc<ControlServer>,
    pub p2p_host: Option<Arc<P2pHostManager>>,
    pub room_ticket: Option<RoomTicket>,
    pub local_media_port: u16,
    pub out_resolution: (u32, u32),
    pub active_fps: u32,
    pub packets_sent: Arc<std::sync::atomic::AtomicU64>,
    pub bytes_sent: Arc<std::sync::atomic::AtomicU64>,
    pub nacks_served: Arc<std::sync::atomic::AtomicU64>,
    threads: Vec<JoinHandle<()>>,
}

impl SenderPipeline {
    pub fn start(
        bind_ip: Ipv4Addr,
        source: SourceInfo,
        preset: Preset,
        fps_target: Option<u32>,
        custom_bitrate_kbps: Option<u32>,
        audio_muted: bool,
        control_server: Arc<ControlServer>,
        preview_sink: Option<PreviewSink>,
        p2p_endpoint: Option<iroh::Endpoint>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        unsafe { let _ = windows::Win32::Media::timeBeginPeriod(1); };

        let packets_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let bytes_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let nacks_served = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let (capture_fps, effective_fps) = match fps_target {
            None => {
                let hz = source.refresh_rate().max(60);
                (hz, hz)
            }
            Some(0) => (0, source.refresh_rate().max(60)),
            Some(n) => (n, n),
        };

        // 1. Cria socket UDP local
        let sock2 = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        sock2.set_reuse_address(true)?;
        let _ = sock2.set_send_buffer_size(4 * 1024 * 1024);
        let _ = sock2.set_recv_buffer_size(4 * 1024 * 1024);
        let bind_addr = SocketAddrV4::new(bind_ip, 0);
        sock2.bind(&socket2::SockAddr::from(bind_addr))?;
        let std_sock: std::net::UdpSocket = sock2.into();
        std_sock.set_read_timeout(Some(Duration::from_millis(50)))?;
        let local_media_port = std_sock.local_addr()?.port();
        tracing::info!("sender: UDP socket bound strictly to {bind_ip}:{local_media_port}");

        // 2. Inicializa dispositivo D3D11 e Captura WGC
        let gpu = Gpu::new().context("falha ao criar contexto GPU D3D11")?;
        let (frame_tx, frame_rx) = crossbeam_channel::bounded(4);
        let cap = WgcCapture::start(&gpu, &source, capture_fps, frame_tx).context("falha ao iniciar WGC")?;

        let initial_size = cap.initial_size;
        let (out_w, out_h) = target_size(initial_size.0, initial_size.1, preset);
        tracing::info!(
            "sender: resolução captura = {}x{}, saída = {}x{} ({:?}) @ {} FPS (capture_fps: {})",
            initial_size.0, initial_size.1, out_w, out_h, preset, effective_fps, capture_fps
        );

        let mut scaler = Scaler::new(&gpu, initial_size, (out_w, out_h), effective_fps)
            .context("falha ao inicializar Scaler GPU")?;

        // 3. Inicializa Encoder H.264
        // Bitrate adaptado dinamicamente para altas taxas de quadros (>60 FPS) ou customizado
        let base_bitrate = if let Some(kbps) = custom_bitrate_kbps {
            kbps * 1_000
        } else {
            preset.default_bitrate()
        };
        let bitrate = if effective_fps > 60 {
            let scale_factor = (effective_fps as f32 / 60.0).sqrt();
            (base_bitrate as f32 * scale_factor) as u32
        } else {
            base_bitrate
        };

        let enc_cfg = EncoderConfig {
            width: out_w,
            height: out_h,
            fps: effective_fps,
            bitrate,
        };

        let (enc_tx, enc_rx) = crossbeam_channel::bounded::<EncodedFrame>(32);
        let encoder = Arc::new(H264Encoder::start(
            &gpu,
            enc_cfg,
            true,
            Box::new(move |ef| {
                let _ = enc_tx.try_send(ef);
            }),
        ).context("falha ao inicializar H264Encoder")?);

        // 4. Inicializa Captura de Áudio WASAPI com exclusão do Discord
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<f32>>(8);
        let audio_target = match &source {
            SourceInfo::Monitor { .. } => AudioTarget::System,
            SourceInfo::Window { pid, .. } => AudioTarget::Window { pid: *pid },
        };
        let audio_cap = AudioCapturer::start(
            audio_target,
            &["discord.exe".into(), "discordcanary.exe".into(), "discordptb.exe".into()],
            Box::new(move |pcm| {
                let _ = audio_tx.try_send(pcm.to_vec());
            }),
        ).context("falha ao iniciar AudioCapturer WASAPI")?;
        audio_cap.set_muted(audio_muted);

        let ring = Arc::new(Mutex::new(RetransmitRing::new(512)));
        let min_bitrate = (bitrate / 3).max(3_000_000).min(bitrate);
        let congestion_controller = Arc::new(Mutex::new(CongestionController::new(min_bitrate, bitrate, bitrate)));

        // Inicializa gerenciador P2P Nativo Iroh se fornecido
        let (p2p_host, room_ticket) = if let Some(endpoint) = p2p_endpoint {
            let host_mgr = Arc::new(P2pHostManager::new());
            let ticket = match RoomTicket::new(&endpoint.addr()) {
                Ok(t) => {
                    tracing::info!("sender: Ticket P2P da sala gerado com sucesso: {}", t.raw);
                    Some(t)
                }
                Err(e) => {
                    tracing::warn!("sender: falha ao gerar RoomTicket: {e:#}");
                    None
                }
            };

            let v_offer = VideoOffer {
                width: out_w,
                height: out_h,
                fps: effective_fps,
            };
            let a_offer = Some(AudioOffer {
                sample_rate: 48000,
                channels: 2,
            });

            let enc_kf = encoder.clone();
            let kf_req = Arc::new(move || {
                enc_kf.request_keyframe();
            });

            let cc_p2p = congestion_controller.clone();
            let enc_p2p = encoder.clone();
            let on_telemetry_p2p: Arc<dyn Fn(TelemetryReport) + Send + Sync> = Arc::new(move |rep| {
                let loss = rep.loss_pct_x10 as f32 / 10.0;
                let rtt = rep.rtt_ms as u32;
                if let Some(new_br) = cc_p2p.lock().on_telemetry(rtt, loss) {
                    enc_p2p.set_bitrate(new_br);
                }
            });

            host_mgr.clone().start_listener(
                endpoint,
                v_offer,
                a_offer,
                ring.clone(),
                kf_req,
                nacks_served.clone(),
                on_telemetry_p2p,
                stop.clone(),
            );

            (Some(host_mgr), ticket)
        } else {
            (None, None)
        };

        // --- Thread 1: WGC Capture -> Scaler -> Encoder ---
        let st1 = stop.clone();
        let enc1 = encoder.clone();
        let wgc_thread = std::thread::Builder::new().name("sender-wgc-pump".into()).spawn(move || {
            while !st1.load(Ordering::SeqCst) {
                match frame_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(frame) => match scaler.process(&frame.texture) {
                        Ok(nv12) => {
                            enc1.submit(nv12, frame.pts_100ns);
                        }
                        Err(e) => {
                            tracing::warn!("sender: scaler falhou: {e:#}");
                        }
                    },
                    Err(_) => {}
                }
            }
            drop(cap);
        })?;

        // --- Thread 2: Video Packets -> Interleaved Round-Robin Dispatch (UDP + Iroh P2P) + Ring Buffer ---
        let st2 = stop.clone();
        let ring2 = ring.clone();
        let cc2 = congestion_controller.clone();
        let ctrl2 = control_server.clone();
        let p2p_host_v = p2p_host.clone();
        let enc2 = encoder.clone();
        let sock_v = std_sock.try_clone()?;
        let pkts_sent_v = packets_sent.clone();
        let bytes_sent_v = bytes_sent.clone();

        let (preview_tx, preview_rx) = crossbeam_channel::bounded::<(bool, u32, Vec<u8>)>(8);
        let st_prev = stop.clone();
        let prev_sink = preview_sink.clone();
        let preview_thread = std::thread::Builder::new()
            .name("sender-preview-dispatch".into())
            .spawn(move || {
                while !st_prev.load(Ordering::SeqCst) {
                    match preview_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok((key, pts, data)) => {
                            if let Some(ref sink) = prev_sink {
                                sink(key, pts, data);
                            }
                        }
                        Err(_) => {}
                    }
                }
            })?;

        let has_preview = preview_sink.is_some();
        let p_tx = preview_tx.clone();
        let video_thread = std::thread::Builder::new().name("sender-video-dispatch".into()).spawn(move || {
            let mut frame_seq: u32 = 1;
            let mut last_viewers_cnt: usize = 0;

            while !st2.load(Ordering::SeqCst) {
                match enc_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(ef) => {
                        let fid = frame_seq;
                        frame_seq = frame_seq.wrapping_add(1);
                        let ts_90k = ((ef.pts_100ns as u64).wrapping_mul(9) / 1000) as u32;

                        if has_preview {
                            let _ = p_tx.try_send((ef.key, ts_90k, ef.data.clone()));
                        }

                        let packets = packetize(KIND_VIDEO, fid, ef.key, ts_90k, &ef.data);
                        let fec_packets = generate_fec(fid, ef.key, ts_90k, &packets, DEFAULT_FEC_BLOCK_SIZE);

                        let udp_viewers = ctrl2.viewer_media_addrs();
                        let p2p_viewers = if let Some(ref host) = p2p_host_v {
                            host.active_viewers()
                        } else {
                            Vec::new()
                        };

                        let total_viewers = udp_viewers.len() + p2p_viewers.len();
                        if total_viewers == 0 {
                            continue;
                        }

                        // Ajuste dinâmico de bitrate baseado no número total de espectadores
                        if total_viewers != last_viewers_cnt {
                            last_viewers_cnt = total_viewers;
                            let target_br = calculate_adaptive_bitrate(bitrate, total_viewers);
                            cc2.lock().set_max_bitrate(target_br);
                            let cur_br = cc2.lock().current_bitrate();
                            enc2.set_bitrate(cur_br);
                            tracing::info!("sender: ajustando teto do bitrate adaptativo para {target_br} bps (atual: {cur_br} bps, {total_viewers} espectadores)");
                        }

                        {
                            let mut r = ring2.lock();
                            for (i, p) in packets.iter().enumerate() {
                                r.push(fid, i as u16, p.clone());
                            }
                        }

                        // Monta lista intercalando blocos de dados com pacotes de paridade FEC imediata
                        let mut dispatch_list: Vec<&Vec<u8>> = Vec::with_capacity(packets.len() + fec_packets.len());
                        let mut fec_idx = 0;
                        for (i, p) in packets.iter().enumerate() {
                            dispatch_list.push(p);
                            if (i + 1) % DEFAULT_FEC_BLOCK_SIZE == 0 || i == packets.len() - 1 {
                                if fec_idx < fec_packets.len() {
                                    dispatch_list.push(&fec_packets[fec_idx]);
                                    fec_idx += 1;
                                }
                            }
                        }

                        // Intercalação + Rotação de peers (Round-Robin) para evitar viés de fila
                        let num_udp = udp_viewers.len();
                        let num_p2p = p2p_viewers.len();
                        let offset = (fid as usize) % total_viewers;
                        let mut bytes_in_burst = 0usize;

                        for p in dispatch_list {
                            let bytes_p2p = if num_p2p > 0 {
                                Some(Bytes::copy_from_slice(p))
                            } else {
                                None
                            };

                            for i in 0..total_viewers {
                                let idx = (offset + i) % total_viewers;
                                if idx < num_udp {
                                    let target = udp_viewers[idx];
                                    let _ = sock_v.send_to(p, target);
                                } else {
                                    let p2p_idx = idx - num_udp;
                                    if let Some(ref b) = bytes_p2p {
                                        let _ = p2p_viewers[p2p_idx].send_datagram(b.clone());
                                    }
                                }
                                bytes_in_burst += p.len();
                                pkts_sent_v.fetch_add(1, Ordering::Relaxed);
                                bytes_sent_v.fetch_add(p.len() as u64, Ordering::Relaxed);
                            }

                            // Pacing de precisão sub-milissegundo:
                            // A cada bloco de ~8 KB enviados, pausa de 200µs via spin-wait sem ceder quantum
                            if bytes_in_burst >= 8192 {
                                bytes_in_burst = 0;
                                let spin_start = std::time::Instant::now();
                                while spin_start.elapsed().as_micros() < 200 {
                                    std::hint::spin_loop();
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        })?;

        // --- Thread 3: Audio PCM -> Opus Encode -> Broadcast (UDP + Iroh P2P) ---
        let st3 = stop.clone();
        let ctrl3 = control_server.clone();
        let p2p_host_a = p2p_host.clone();
        let sock_a = std_sock.try_clone()?;
        let pkts_sent_a = packets_sent.clone();
        let bytes_sent_a = bytes_sent.clone();

        let audio_thread = std::thread::Builder::new().name("sender-audio-dispatch".into()).spawn(move || {
            let mut opus_enc = match OpusEnc::new(128_000) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("sender: falha ao criar OpusEnc: {e:#}");
                    return;
                }
            };
            let mut audio_seq: u32 = 1;
            let mut ts_90k: u32 = 0;
            while !st3.load(Ordering::SeqCst) {
                match audio_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(pcm) => {
                        let udp_viewers = ctrl3.viewer_media_addrs();
                        let p2p_viewers = if let Some(ref host) = p2p_host_a {
                            host.active_viewers()
                        } else {
                            Vec::new()
                        };

                        if udp_viewers.is_empty() && p2p_viewers.is_empty() {
                            continue;
                        }

                        if let Ok(opus_data) = opus_enc.encode(&pcm) {
                            let fid = audio_seq;
                            audio_seq = audio_seq.wrapping_add(1);
                            ts_90k = ts_90k.wrapping_add(1800); // 960 amostras @ 48kHz = 20ms = 1800 ticks de 90kHz
                            let pkts = packetize(KIND_AUDIO, fid, false, ts_90k, opus_data);
                            for p in pkts {
                                for addr in &udp_viewers {
                                    let _ = sock_a.send_to(&p, *addr);
                                    pkts_sent_a.fetch_add(1, Ordering::Relaxed);
                                    bytes_sent_a.fetch_add(p.len() as u64, Ordering::Relaxed);
                                }
                                if !p2p_viewers.is_empty() {
                                    let b = Bytes::copy_from_slice(&p);
                                    for v in &p2p_viewers {
                                        let _ = v.send_datagram(b.clone());
                                        pkts_sent_a.fetch_add(1, Ordering::Relaxed);
                                        bytes_sent_a.fetch_add(p.len() as u64, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        })?;

        // --- Thread 4: UDP Listener for NACKs and PLIs com PLI Coalescing (250ms) ---
        let st4 = stop.clone();
        let ring4 = ring.clone();
        let cc_udp = congestion_controller.clone();
        let enc4 = encoder.clone();
        let sock_rx = std_sock.try_clone()?;
        let nacks_cnt = nacks_served.clone();

        let listener_thread = std::thread::Builder::new().name("sender-udp-rx".into()).spawn(move || {
            let mut buf = [0u8; 2048];
            let mut last_pli = std::time::Instant::now() - Duration::from_secs(5);
            let mut pending_pli = false;

            while !st4.load(Ordering::SeqCst) {
                // Checa se há PLI acumulado pronto para disparar no fim da janela de 250ms
                if pending_pli && std::time::Instant::now().duration_since(last_pli) >= Duration::from_millis(250) {
                    last_pli = std::time::Instant::now();
                    pending_pli = false;
                    tracing::info!("sender: disparando IDR acumulado por PLI coalescing (fim da janela de 250ms)");
                    enc4.request_keyframe();
                }

                match sock_rx.recv_from(&mut buf) {
                    Ok((n, from_addr)) => {
                        if let Some((h, payload)) = MediaHeader::parse(&buf[..n]) {
                            match h.kind {
                                KIND_NACK => {
                                    let missing = parse_nack_payload(payload);
                                    let to_resend: Vec<std::sync::Arc<Vec<u8>>> = {
                                        let ring = ring4.lock();
                                        missing
                                            .into_iter()
                                            .filter_map(|idx| ring.get(h.frame_id, idx))
                                            .collect()
                                     };
                                    let count = to_resend.len();
                                    for pkt in to_resend {
                                        let _ = sock_rx.send_to(&pkt, from_addr);
                                    }
                                    nacks_cnt.fetch_add(count as u64, Ordering::Relaxed);
                                }
                                KIND_PLI => {
                                    let now = std::time::Instant::now();
                                    if now.duration_since(last_pli) >= Duration::from_millis(250) {
                                        last_pli = now;
                                        pending_pli = false;
                                        tracing::info!("sender: PLI recebido de {from_addr}, gerando keyframe IDR imediato");
                                        enc4.request_keyframe();
                                    } else {
                                        pending_pli = true;
                                        tracing::debug!("sender: PLI de {from_addr} agrupado (coalescing em janela de 250ms)");
                                    }
                                }
                                KIND_TELEMETRY => {
                                    if let Some(rep) = TelemetryReport::from_bytes(payload) {
                                        let loss = rep.loss_pct_x10 as f32 / 10.0;
                                        let rtt = rep.rtt_ms as u32;
                                        if let Some(new_br) = cc_udp.lock().on_telemetry(rtt, loss) {
                                            enc4.set_bitrate(new_br);
                                        }
                                    }
                                }
                                KIND_HELLO => {
                                    tracing::debug!("sender: HELLO recebido de {from_addr}");
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => {}
                }
            }
        })?;

        Ok(Self {
            stop,
            encoder,
            audio_cap,
            control_server,
            p2p_host,
            room_ticket,
            local_media_port,
            out_resolution: (out_w, out_h),
            active_fps: effective_fps,
            packets_sent,
            bytes_sent,
            nacks_served,
            threads: vec![wgc_thread, video_thread, audio_thread, listener_thread, preview_thread],
        })
    }

    pub fn set_audio_muted(&self, muted: bool) {
        self.audio_cap.set_muted(muted);
        let ctrl = self.control_server.clone();
        let p2p = self.p2p_host.clone();
        tauri::async_runtime::spawn(async move {
            ctrl.broadcast(Control::AudioState { enabled: !muted }).await;
            if let Some(ref host) = p2p {
                host.broadcast_control(Control::AudioState { enabled: !muted }).await;
            }
        });
    }

    pub fn request_keyframe(&self) {
        self.encoder.request_keyframe();
    }

    pub fn keyframe_requester(&self) -> Arc<dyn Fn() + Send + Sync> {
        let enc = self.encoder.clone();
        Arc::new(move || {
            enc.request_keyframe();
        })
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.packets_sent.load(Ordering::Relaxed),
            self.bytes_sent.load(Ordering::Relaxed),
            self.nacks_served.load(Ordering::Relaxed),
        )
    }

    pub fn viewers_count(&self) -> usize {
        let udp_cnt = self.control_server.viewers().len();
        let p2p_cnt = self.p2p_host.as_ref().map(|h| h.viewers_count()).unwrap_or(0);
        udp_cnt + p2p_cnt
    }

    pub fn viewers_info(&self) -> Vec<String> {
        let mut list = Vec::new();
        for v in self.control_server.viewers() {
            list.push(format!("{} ({})", v.nick, v.ip));
        }
        if let Some(ref host) = self.p2p_host {
            for (id, nick) in host.viewers_info() {
                list.push(format!("{} (P2P:{})", nick, &id.to_string()[..8]));
            }
        }
        list
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        unsafe { let _ = windows::Win32::Media::timeEndPeriod(1); };
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for SenderPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}
