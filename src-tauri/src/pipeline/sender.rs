//! Orquestrador de transmissão P2P (Sender): captura WGC, scaler GPU, encoder H.264/HEVC/AV1,
//! captura de áudio WASAPI com filtro do Discord, codificação Opus com RED/FEC e transporte Iroh P2P.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use parking_lot::{Mutex, RwLock};

use crate::audio::opus_enc::OpusEnc;
use crate::audio::{AudioCapturer, AudioTarget};
use crate::capture::sources::SourceInfo;
use crate::capture::wgc::WgcCapture;
use crate::net::p2p::{P2pHostManager, RoomTicket};
use crate::net::protocol::*;
use crate::net::transport::{
    generate_fec, packetize, CongestionController, RetransmitRing,
};
use crate::video::d3d::Gpu;
use crate::video::encoder::{EncodedFrame, EncoderConfig, H264Encoder, VideoCodec};
use crate::video::resolution::{calculate_adaptive_bitrate, target_size, Preset};
use crate::video::scaler::Scaler;

pub type PreviewSink = Arc<dyn Fn(bool, u32, Vec<u8>) + Send + Sync>;

pub struct SenderPipeline {
    stop: Arc<AtomicBool>,
    gpu: Gpu,
    wgc_stop: Arc<AtomicBool>,
    wgc_thread: Option<JoinHandle<()>>,
    enc_tx: crossbeam_channel::Sender<EncodedFrame>,
    encoder_holder: Arc<RwLock<Arc<H264Encoder>>>,
    congestion_controller: Arc<Mutex<CongestionController>>,
    base_target_bitrate: Arc< std::sync::atomic::AtomicU32 >,
    audio_cap: AudioCapturer,
    audio_tx: crossbeam_channel::Sender<Vec<f32>>,
    pub p2p_endpoint: iroh::Endpoint,
    pub p2p_host: Arc<P2pHostManager>,
    pub room_ticket: Option<RoomTicket>,
    pub out_resolution: (u32, u32),
    pub active_fps: u32,
    pub active_codec: VideoCodec,
    pub packets_sent: Arc<std::sync::atomic::AtomicU64>,
    pub bytes_sent: Arc<std::sync::atomic::AtomicU64>,
    pub nacks_served: Arc<std::sync::atomic::AtomicU64>,
    threads: Vec<JoinHandle<()>>,
}

impl SenderPipeline {
    pub fn start(
        source: SourceInfo,
        preset: Preset,
        fps_target: Option<u32>,
        custom_bitrate_kbps: Option<u32>,
        codec_pref: Option<String>,
        audio_muted: bool,
        preview_sink: Option<PreviewSink>,
        p2p_endpoint: iroh::Endpoint,
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

        // 1. Inicializa dispositivo D3D11 e Captura WGC
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

        // 2. Inicializa Encoder de Vídeo
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
        let base_target_bitrate = Arc::new(std::sync::atomic::AtomicU32::new(bitrate));

        let requested_codec = codec_pref
            .as_deref()
            .map(VideoCodec::from_str_loose)
            .unwrap_or(VideoCodec::Auto);

        let enc_cfg = EncoderConfig {
            width: out_w,
            height: out_h,
            fps: effective_fps,
            bitrate,
            codec: requested_codec,
        };

        let (enc_tx, enc_rx) = crossbeam_channel::bounded::<EncodedFrame>(32);
        let enc_tx_clone = enc_tx.clone();
        let encoder = Arc::new(H264Encoder::start(
            &gpu,
            enc_cfg,
            true,
            Box::new(move |ef| {
                let _ = enc_tx.try_send(ef);
            }),
        ).context("falha ao inicializar H264Encoder")?);
        let active_codec = encoder.codec;
        let encoder_holder = Arc::new(RwLock::new(encoder.clone()));

        // 3. Inicializa Captura de Áudio WASAPI com exclusão do Discord
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<f32>>(8);
        let audio_tx_clone = audio_tx.clone();
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

        // Ring buffer ampliado (2048 pacotes = ~1 a 4s de histórico) para cobrir NACKs em rotas de internet altas
        let ring = Arc::new(Mutex::new(RetransmitRing::new(2048)));
        let min_bitrate = (bitrate / 6).clamp(1_200_000, 2_500_000).min(bitrate);
        let congestion_controller = Arc::new(Mutex::new(CongestionController::new(min_bitrate, bitrate, bitrate)));

        // 4. Inicializa gerenciador P2P Nativo Iroh
        let p2p_host = Arc::new(P2pHostManager::new());
        let room_ticket = match RoomTicket::new(&p2p_endpoint.addr()) {
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
            codec: active_codec.as_str().to_string(),
        };
        let a_offer = Some(AudioOffer {
            sample_rate: 48000,
            channels: 2,
        });

        let enc_kf = encoder_holder.clone();
        let kf_req = Arc::new(move || {
            enc_kf.read().request_keyframe();
        });

        let cc_p2p = congestion_controller.clone();
        let enc_p2p = encoder_holder.clone();
        let on_telemetry_p2p: Arc<dyn Fn(TelemetryReport) + Send + Sync> = Arc::new(move |rep| {
            let loss = rep.loss_pct_x10 as f32 / 10.0;
            let rtt = rep.rtt_ms as u32;
            if let Some(new_br) = cc_p2p.lock().on_telemetry(rtt, loss) {
                enc_p2p.read().set_bitrate(new_br);
            }
        });

        p2p_host.clone().start_listener(
            p2p_endpoint.clone(),
            v_offer,
            a_offer,
            ring.clone(),
            kf_req,
            nacks_served.clone(),
            on_telemetry_p2p,
            stop.clone(),
        );

        // --- Thread 1: WGC Capture -> Scaler -> Encoder ---
        let wgc_stop = Arc::new(AtomicBool::new(false));
        let st_wgc = wgc_stop.clone();
        let enc1 = encoder.clone();
        let wgc_thread = std::thread::Builder::new().name("sender-wgc-pump".into()).spawn(move || {
            while !st_wgc.load(Ordering::SeqCst) {
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

        // --- Thread 2: Video Packets -> Adaptive FEC + Interleaved Round-Robin Dispatch (Iroh P2P) ---
        let st2 = stop.clone();
        let ring2 = ring.clone();
        let cc2 = congestion_controller.clone();
        let base_br_v = base_target_bitrate.clone();
        let p2p_host_v = p2p_host.clone();
        let enc2 = encoder_holder.clone();
        let pkts_sent_v = packets_sent.clone();
        let bytes_sent_v = bytes_sent.clone();

        let (preview_tx, preview_rx) = crossbeam_channel::bounded::<(bool, u32, Vec<u8>)>(8);
        let st_prev = stop.clone();
        let prev_sink = preview_sink.clone();
        let preview_thread = std::thread::Builder::new()
            .name("sender-preview-dispatch".into())
            .spawn(move || {
                let mut last_preview = std::time::Instant::now();
                while !st_prev.load(Ordering::SeqCst) {
                    match preview_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok((key, pts, data)) => {
                            // Sempre entrega keyframes instantâneos; delta frames limitados a ~35 FPS para o preview local
                            if key || last_preview.elapsed() >= Duration::from_millis(28) {
                                last_preview = std::time::Instant::now();
                                if let Some(ref sink) = prev_sink {
                                    sink(key, pts, data);
                                }
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

                        let p2p_viewers = p2p_host_v.active_viewers();
                        let total_viewers = p2p_viewers.len();
                        if total_viewers == 0 {
                            continue;
                        }

                        // Ajuste dinâmico de bitrate baseado no número total de espectadores
                        if total_viewers != last_viewers_cnt {
                            last_viewers_cnt = total_viewers;
                            let base_br = base_br_v.load(Ordering::Relaxed);
                            let target_br = calculate_adaptive_bitrate(base_br, total_viewers);
                            let cur_br = {
                                let mut cc = cc2.lock();
                                cc.set_max_bitrate(target_br);
                                cc.current_bitrate()
                            };
                            enc2.read().set_bitrate(cur_br);
                            tracing::info!(
                                "sender: ajustando teto do bitrate adaptativo para {target_br} bps (atual: {cur_br} bps, {total_viewers} espectadores)"
                            );
                        }

                        let fec_block_size = cc2.lock().recommended_fec_block_size(ef.key);
                        let packets = packetize(KIND_VIDEO, fid, ef.key, ts_90k, &ef.data);
                        let fec_packets = generate_fec(fid, ef.key, ts_90k, &packets, fec_block_size);

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
                            if (i + 1) % fec_block_size == 0 || i == packets.len() - 1 {
                                if fec_idx < fec_packets.len() {
                                    dispatch_list.push(&fec_packets[fec_idx]);
                                    fec_idx += 1;
                                }
                            }
                        }

                        // Intercalação + Rotação de peers (Round-Robin) com micro-pacing anti-bufferbloat
                        let offset = (fid as usize) % total_viewers;
                        let mut bytes_in_burst = 0usize;
                        let mut total_frame_bytes = 0usize;
                        let pause_us: u128 = if ef.key { 280 } else { 180 };

                        for p in dispatch_list {
                            let bytes_p2p = Bytes::copy_from_slice(p);
                            for i in 0..total_viewers {
                                let idx = (offset + i) % total_viewers;
                                let _ = p2p_viewers[idx].send_datagram(bytes_p2p.clone());
                                bytes_in_burst += p.len();
                                total_frame_bytes += p.len();
                                pkts_sent_v.fetch_add(1, Ordering::Relaxed);
                                bytes_sent_v.fetch_add(p.len() as u64, Ordering::Relaxed);
                            }

                            // Primeiros 12 KB saem imediatamente; acima disso aplica pacing suave a cada 6 KB
                            // para não estourar filas de roteadores Wi-Fi/ISP em Keyframes grandes
                            if total_frame_bytes > 12_288 && bytes_in_burst >= 6144 {
                                bytes_in_burst = 0;
                                let spin_start = std::time::Instant::now();
                                while spin_start.elapsed().as_micros() < pause_us {
                                    std::hint::spin_loop();
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        })?;

        // --- Thread 3: Audio PCM -> Opus Encode (In-Band FEC) + Audio RED -> Broadcast P2P ---
        let st3 = stop.clone();
        let p2p_host_a = p2p_host.clone();
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
            let mut prev_opus: Option<Vec<u8>> = None;

            while !st3.load(Ordering::SeqCst) {
                match audio_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(pcm) => {
                        let p2p_viewers = p2p_host_a.active_viewers();
                        if p2p_viewers.is_empty() {
                            continue;
                        }

                        if let Ok(opus_data) = opus_enc.encode(&pcm) {
                            let fid = audio_seq;
                            audio_seq = audio_seq.wrapping_add(1);
                            ts_90k = ts_90k.wrapping_add(1800); // 960 amostras @ 48kHz = 20ms = 1800 ticks de 90kHz

                            let pkt = pack_audio_red(prev_opus.as_deref(), opus_data, fid, ts_90k);
                            prev_opus = Some(opus_data.to_vec());

                            let b = Bytes::from(pkt);
                            let pkt_len = b.len() as u64;
                            for v in &p2p_viewers {
                                let _ = v.send_datagram(b.clone());
                                pkts_sent_a.fetch_add(1, Ordering::Relaxed);
                                bytes_sent_a.fetch_add(pkt_len, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        })?;

        Ok(Self {
            stop,
            gpu,
            wgc_stop,
            wgc_thread: Some(wgc_thread),
            enc_tx: enc_tx_clone,
            encoder_holder,
            congestion_controller,
            base_target_bitrate,
            audio_cap,
            audio_tx: audio_tx_clone,
            p2p_endpoint,
            p2p_host,
            room_ticket,
            out_resolution: (out_w, out_h),
            active_fps: effective_fps,
            active_codec,
            packets_sent,
            bytes_sent,
            nacks_served,
            threads: vec![video_thread, audio_thread, preview_thread],
        })
    }

    pub fn switch_source(
        &mut self,
        source: SourceInfo,
        preset: Preset,
        fps_target: Option<u32>,
        custom_bitrate_kbps: Option<u32>,
        codec_pref: Option<String>,
        audio_muted: bool,
    ) -> Result<()> {
        tracing::info!("sender: Iniciando troca de fonte de transmissão...");

        // 1. Para a thread WGC antiga e aguarda finalização
        self.wgc_stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.wgc_thread.take() {
            let _ = t.join();
        }

        // 2. Determina framerate e inicializa nova Captura WGC para obter as dimensões reais
        let (capture_fps, effective_fps) = match fps_target {
            None => {
                let hz = source.refresh_rate().max(60);
                (hz, hz)
            }
            Some(0) => (0, source.refresh_rate().max(60)),
            Some(n) => (n, n),
        };

        let (frame_tx, frame_rx) = crossbeam_channel::bounded(4);
        let cap = WgcCapture::start(&self.gpu, &source, capture_fps, frame_tx)
            .context("falha ao criar novo WgcCapture durante switch")?;

        let initial_size = cap.initial_size;
        let (out_w, out_h) = target_size(initial_size.0, initial_size.1, preset);

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
        self.base_target_bitrate.store(bitrate, Ordering::Relaxed);
        let viewers_cnt = self.p2p_host.viewers_count().max(1);
        let adaptive_max = calculate_adaptive_bitrate(bitrate, viewers_cnt);
        let initial_br = {
            let mut cc = self.congestion_controller.lock();
            cc.set_max_bitrate(adaptive_max);
            cc.current_bitrate()
        };

        let requested_codec = codec_pref
            .as_deref()
            .map(VideoCodec::from_str_loose)
            .unwrap_or(VideoCodec::Auto);

        let enc_cfg = EncoderConfig {
            width: out_w,
            height: out_h,
            fps: effective_fps,
            bitrate: initial_br,
            codec: requested_codec,
        };

        // 3. Cria novo encoder e atualiza holder
        let enc_tx = self.enc_tx.clone();
        let new_encoder = Arc::new(H264Encoder::start(
            &self.gpu,
            enc_cfg,
            true,
            Box::new(move |ef| {
                let _ = enc_tx.try_send(ef);
            }),
        ).context("falha ao inicializar novo H264Encoder durante switch")?);

        *self.encoder_holder.write() = new_encoder.clone();
        self.out_resolution = (out_w, out_h);
        self.active_fps = effective_fps;
        self.active_codec = new_encoder.codec;

        // 4. Cria novo Scaler
        let mut scaler = Scaler::new(&self.gpu, initial_size, (out_w, out_h), effective_fps)
            .context("falha ao criar novo Scaler durante switch")?;

        // 5. Inicia nova thread WGC
        let wgc_stop = Arc::new(AtomicBool::new(false));
        self.wgc_stop = wgc_stop.clone();
        let st_wgc = wgc_stop;
        let enc1 = new_encoder.clone();

        let wgc_thread = std::thread::Builder::new().name("sender-wgc-pump".into()).spawn(move || {
            while !st_wgc.load(Ordering::SeqCst) {
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
        self.wgc_thread = Some(wgc_thread);

        // 6. Atualiza captura de áudio para a nova fonte
        let audio_tx = self.audio_tx.clone();
        let audio_target = match &source {
            SourceInfo::Monitor { .. } => AudioTarget::System,
            SourceInfo::Window { pid, .. } => AudioTarget::Window { pid: *pid },
        };
        if let Ok(new_audio_cap) = AudioCapturer::start(
            audio_target,
            &["discord.exe".into(), "discordcanary.exe".into(), "discordptb.exe".into()],
            Box::new(move |pcm| {
                let _ = audio_tx.try_send(pcm.to_vec());
            }),
        ) {
            new_audio_cap.set_muted(audio_muted);
            self.audio_cap = new_audio_cap;
        }

        // 7. Notifica todos os espectadores P2P via Control::Reconfigure
        let reconfig = Control::Reconfigure {
            video: VideoOffer {
                width: out_w,
                height: out_h,
                fps: effective_fps,
                codec: new_encoder.codec.as_str().to_string(),
            },
        };
        let p2p = self.p2p_host.clone();
        tauri::async_runtime::spawn(async move {
            p2p.broadcast_control(reconfig).await;
        });

        // 8. Requisita um IDR imediato
        new_encoder.request_keyframe();
        tracing::info!("sender: Troca de fonte de transmissão concluída com sucesso ({out_w}x{out_h} @ {effective_fps}fps)");

        Ok(())
    }

    pub fn set_audio_muted(&self, muted: bool) {
        self.audio_cap.set_muted(muted);
        let p2p = self.p2p_host.clone();
        tauri::async_runtime::spawn(async move {
            p2p.broadcast_control(Control::AudioState { enabled: !muted }).await;
        });
    }

    pub fn request_keyframe(&self) {
        self.encoder_holder.read().request_keyframe();
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.packets_sent.load(Ordering::Relaxed),
            self.bytes_sent.load(Ordering::Relaxed),
            self.nacks_served.load(Ordering::Relaxed),
        )
    }

    pub fn viewers_count(&self) -> usize {
        self.p2p_host.viewers_count()
    }

    pub fn viewers_info(&self) -> Vec<String> {
        let mut list = Vec::new();
        for (id, nick) in self.p2p_host.viewers_info() {
            let n = nick.trim();
            if !n.is_empty() {
                list.push(n.to_string());
            } else {
                list.push(format!("Espectador ({})", &id.to_string()[..6]));
            }
        }
        list
    }

    pub fn stop(&mut self) {
        self.wgc_stop.store(true, Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
        unsafe { let _ = windows::Win32::Media::timeEndPeriod(1); };
        let ep = self.p2p_endpoint.clone();
        tauri::async_runtime::spawn(async move {
            let _ = ep.close().await;
        });
        if let Some(t) = self.wgc_thread.take() {
            let _ = t.join();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for SenderPipeline {
    fn drop(&mut self) {
        if !self.stop.load(Ordering::SeqCst) {
            self.stop();
        }
    }
}
