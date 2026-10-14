//! Orquestrador de recepção (Receiver): conecta ao transmissor via TCP de controle,
//! recebe pacotes UDP de mídia em alta velocidade, remonta frames com NACK / PLI,
//! decodifica áudio Opus e despacha para o WebView2 através de thread dedicada.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
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
use crate::net::session::ViewerClient;
use crate::net::transport::{build_nack, Reassembler, RxEvent};
use crate::pipeline::{base64_encode, StreamEvent};

pub type StreamEventSink = Arc<dyn Fn(StreamEvent) + Send + Sync>;

pub struct ReceiverPipeline {
    stop: Arc<AtomicBool>,
    client: Arc<parking_lot::Mutex<Option<ViewerClient>>>,
    udp_sock: Option<std::net::UdpSocket>,
    p2p_endpoint: Option<iroh::Endpoint>,
    p2p_conn: Option<iroh::endpoint::Connection>,
    p2p_send_ctrl: Arc<tokio::sync::Mutex<Option<iroh::endpoint::SendStream>>>,
    remote_addr: Option<SocketAddr>,
    threads: Vec<JoinHandle<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ReceiverPipeline {
    pub async fn start(
        local_bind_ip: Ipv4Addr,
        sharer_ip: Ipv4Addr,
        sharer_ctrl_port: u16,
        my_id: Uuid,
        my_nick: String,
        event_sink: StreamEventSink,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));

        // 1. Vincula socket UDP local com buffer ampliado
        let sock2 = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        sock2.set_reuse_address(true)?;
        let _ = sock2.set_recv_buffer_size(4 * 1024 * 1024);
        let _ = sock2.set_send_buffer_size(4 * 1024 * 1024);
        let bind_addr = SocketAddrV4::new(local_bind_ip, 0);
        sock2.bind(&socket2::SockAddr::from(bind_addr))?;
        let std_sock: std::net::UdpSocket = sock2.into();
        std_sock.set_read_timeout(Some(Duration::from_millis(2)))?;
        let local_udp_port = std_sock.local_addr()?.port();
        tracing::info!("receiver: UDP socket bound to {local_bind_ip}:{local_udp_port} (buffers=4MB)");

        // Clone do socket para permitir requisições manuais de Keyframe (PLI) vindas do WebCodecs
        let std_sock_clone = std_sock.try_clone()?;

        // 2. Conecta ao host via TCP de controle
        let sink_reconfig = event_sink.clone();
        let sink_audio = event_sink.clone();
        let client = ViewerClient::connect(
            sharer_ip,
            sharer_ctrl_port,
            my_id,
            my_nick,
            local_udp_port,
            Arc::new(move |offer| {
                sink_reconfig(StreamEvent::Config {
                    width: offer.width,
                    height: offer.height,
                    fps: offer.fps,
                    codec: offer.codec.clone(),
                });
            }),
            Arc::new(move |enabled| {
                sink_audio(StreamEvent::AudioState { enabled });
            }),
        )
        .await
        .context("falha ao conectar ao anfitrião via controle")?;

        let remote_media_port = client.remote_media_port;
        let initial_video = client.video_offer.clone();
        let remote_addr = SocketAddr::V4(SocketAddrV4::new(sharer_ip, remote_media_port));
        let client_holder = Arc::new(parking_lot::Mutex::new(Some(client)));

        // 3. Envia datagrama KIND_HELLO para abrir furos em firewalls/NAT
        let hello = control_packet(KIND_HELLO, 0, &[]);
        let _ = std_sock.send_to(&hello, remote_addr);

        // Notifica o frontend com a resolução inicial
        event_sink(StreamEvent::Config {
            width: initial_video.width,
            height: initial_video.height,
            fps: initial_video.fps,
            codec: initial_video.codec.clone(),
        });

        // 4. Filas desacopladas para que a recepção de pacotes UDP nunca bloqueie no IPC
        let (video_tx, video_rx) = crossbeam_channel::bounded::<(bool, u32, Vec<u8>)>(64);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);

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

        // --- Thread B: Loop de recepção e remontagem UDP sem bloqueio com Telemetria periódica ---
        let st_udp = stop.clone();
        let sink_telem = event_sink.clone();
        let udp_thread = std::thread::Builder::new()
            .name("receiver-udp-rx".into())
            .spawn(move || {
                let mut reassembler = Reassembler::new();
                let mut opus_dec = match OpusDec::new() {
                    Ok(d) => Some(d),
                    Err(e) => {
                        tracing::warn!("receiver: falha ao inicializar OpusDec: {e:#}");
                        None
                    }
                };

                let start_instant = Instant::now();
                let mut last_tick = Instant::now();
                let mut last_telem = Instant::now();
                let mut last_frames_delivered: u64 = 0;
                let mut last_pkts_rx: u64 = 0;
                let mut last_pkts_lost: u64 = 0;
                let mut buf = [0u8; 2048];

                while !st_udp.load(Ordering::SeqCst) {
                    let now_ms = start_instant.elapsed().as_millis() as u64;

                    // Lê pacotes UDP (drena enquanto houver pacotes no buffer)
                    let mut read_any = false;
                    for _ in 0..16 {
                        match std_sock.recv_from(&mut buf) {
                            Ok((n, from)) => {
                                read_any = true;
                                if from == remote_addr {
                                    if let Some((h, payload)) = MediaHeader::parse(&buf[..n]) {
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
                                                            let _ = std_sock.send_to(&nack_pkt, remote_addr);
                                                        }
                                                        RxEvent::RequestKey => {
                                                            let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                                                            let _ = std_sock.send_to(&pli_pkt, remote_addr);
                                                        }
                                                    }
                                                }
                                            }
                                            KIND_AUDIO => {
                                                if let Some(dec) = &mut opus_dec {
                                                    if let Ok(pcm) = dec.decode(payload) {
                                                        let bytes: &[u8] = unsafe {
                                                            std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 4)
                                                        };
                                                        let _ = audio_tx.try_send(bytes.to_vec());
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                                break;
                            }
                            Err(_) => {
                                break;
                            }
                        }
                    }

                    // Tick periódico para detecção de pacotes perdidos (NACK / PLI)
                    if last_tick.elapsed() >= Duration::from_millis(4) {
                        last_tick = Instant::now();
                        let tick_events = reassembler.tick(now_ms);
                        for ev in tick_events {
                            match ev {
                                RxEvent::Nack { frame_id, missing } => {
                                    let nack_pkt = build_nack(frame_id, &missing);
                                    let _ = std_sock.send_to(&nack_pkt, remote_addr);
                                }
                                RxEvent::RequestKey => {
                                    let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                                    let _ = std_sock.send_to(&pli_pkt, remote_addr);
                                }
                                _ => {}
                            }
                        }
                    }

                    // Telemetria periódica a cada 2 segundos enviada para o anfitrião e para a UI
                    if last_telem.elapsed() >= Duration::from_millis(2000) {
                        let elapsed_sec = last_telem.elapsed().as_secs_f32();
                        last_telem = Instant::now();
                        let stats = reassembler.stats();

                        let frames_delta = stats.frames_delivered.saturating_sub(last_frames_delivered);
                        last_frames_delivered = stats.frames_delivered;
                        let effective_fps = (frames_delta as f32 / elapsed_sec.max(0.001)).round() as u32;

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

                        // Envia relatório de telemetria de volta para o host (ajuste AIMD de bitrate)
                        let telem_report = TelemetryReport {
                            rtt_ms: stats.rtt_ms.max(1) as u16,
                            loss_pct_x10: ((loss_pct * 10.0) as u16).min(1000),
                            nacks_sent: (stats.nacks_sent as u16).min(u16::MAX),
                            fec_recovered: (stats.fec_recovered as u16).min(u16::MAX),
                        };
                        let telem_pkt = control_packet(KIND_TELEMETRY, 0, &telem_report.to_bytes());
                        let _ = std_sock.send_to(&telem_pkt, remote_addr);

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

                    if !read_any {
                        std::hint::spin_loop();
                    }
                }
            })?;

        Ok(Self {
            stop,
            client: client_holder,
            udp_sock: Some(std_sock_clone),
            p2p_endpoint: None,
            p2p_conn: None,
            p2p_send_ctrl: Arc::new(tokio::sync::Mutex::new(None)),
            remote_addr: Some(remote_addr),
            threads: vec![dispatch_thread, udp_thread],
            tasks: Vec::new(),
        })
    }

    /// Modo 2: Conexão P2P Nativa sobre Iroh (Internet direta via RoomTicket sem VPN).
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

        // 2. Conecta ao host via Iroh (QUIC + TLS 1.3 + STUN)
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

        // 4. Filas desacopladas para vídeo e áudio
        let (video_tx, video_rx) = crossbeam_channel::bounded::<(bool, u32, Vec<u8>)>(64);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);

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

        // --- Tarefa D: Escuta e gerenciamento de conexões Mesh Relay locais ---
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

        // --- Tarefa B: Loop de recepção e remontagem de Datagramas QUIC com Telemetria e Relé Mesh ---
        let st_p2p = stop.clone();
        let sink_telem = event_sink.clone();
        let conn_rx = conn.clone();
        let ep_mesh_conn = endpoint.clone();
        let mesh_peers_fwd = mesh_peers.clone();
        let mesh_dg_tx_mesh = mesh_dg_tx.clone();
        let p2p_task = tokio::spawn(async move {
            let mut reassembler = Reassembler::new();
            let mut opus_dec = match OpusDec::new() {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!("receiver_p2p: falha ao inicializar OpusDec: {e:#}");
                    None
                }
            };

            let start_instant = Instant::now();
            let mut tick_interval = tokio::time::interval(Duration::from_millis(4));
            let mut telem_interval = tokio::time::interval(Duration::from_millis(2000));
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
                                            // Relé Mesh Assistido: retransmite aos peers mesh conectados
                                            let peers = mesh_peers_fwd.lock().clone();
                                            for p in peers {
                                                let _ = p.send_datagram(dg.clone());
                                            }
                                        }
                                        KIND_AUDIO => {
                                            if let Some(dec) = &mut opus_dec {
                                                if let Ok(pcm) = dec.decode(payload) {
                                                    let bytes: &[u8] = unsafe {
                                                        std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 4)
                                                    };
                                                    let _ = audio_tx.try_send(bytes.to_vec());
                                                }
                                            }
                                            let peers = mesh_peers_fwd.lock().clone();
                                            for p in peers {
                                                let _ = p.send_datagram(dg.clone());
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
                        // Datagrama recebido de peer mesh assistido: alimenta o remontador sem reencaminhar
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
                                    if let Some(dec) = &mut opus_dec {
                                        if let Ok(pcm) = dec.decode(payload) {
                                            let bytes: &[u8] = unsafe {
                                                std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 4)
                                            };
                                            let _ = audio_tx.try_send(bytes.to_vec());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ = tick_interval.tick() => {
                        let now_ms = start_instant.elapsed().as_millis() as u64;
                        let tick_events = reassembler.tick(now_ms);
                        for ev in tick_events {
                            match ev {
                                RxEvent::Nack { frame_id, missing } => {
                                    let nack_pkt = build_nack(frame_id, &missing);
                                    let _ = conn_rx.send_datagram(Bytes::from(nack_pkt));
                                }
                                RxEvent::RequestKey => {
                                    let pli_pkt = control_packet(KIND_PLI, 0, &[]);
                                    let _ = conn_rx.send_datagram(Bytes::from(pli_pkt));
                                }
                                _ => {}
                            }
                        }
                    }
                    _ = telem_interval.tick() => {
                        let stats = reassembler.stats();
                        let frames_delta = stats.frames_delivered.saturating_sub(last_frames_delivered);
                        last_frames_delivered = stats.frames_delivered;
                        let effective_fps = (frames_delta as f32 / 2.0).round() as u32;

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

                        // Envia relatório de telemetria de volta para o host (ajuste AIMD de bitrate)
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
            client: Arc::new(parking_lot::Mutex::new(None)),
            udp_sock: None,
            p2p_endpoint: Some(endpoint),
            p2p_conn: Some(conn),
            p2p_send_ctrl,
            remote_addr: None,
            threads: vec![dispatch_thread],
            tasks: vec![ctrl_task, p2p_task, mesh_listener_task],
        })
    }

    pub fn request_keyframe(&self) {
        let pli_pkt = control_packet(KIND_PLI, 0, &[]);
        if let (Some(sock), Some(addr)) = (&self.udp_sock, self.remote_addr) {
            let _ = sock.send_to(&pli_pkt, addr);
        }
        if let Some(conn) = &self.p2p_conn {
            let _ = conn.send_datagram(Bytes::from(pli_pkt));
        }
    }

    pub async fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut client_opt = self.client.lock().take();
        if let Some(mut client) = client_opt.take() {
            client.close().await;
        }
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

        // Aguarda para a sessão se estabilizar
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(host_mgr.viewers_count(), 1, "Host deve ter 1 espectador ativo");
        assert!(pli_called.load(Ordering::SeqCst), "Host deve ter recebido pedido de Keyframe inicial");

        let viewers = host_mgr.active_viewers();
        if let Some(v) = viewers.first() {
            eprintln!("HOST VIEWER max_datagram_size: {:?}", v.conn.max_datagram_size());
        }

        // Envia um frame de vídeo fragmentado para o espectador
        let frame_data = vec![0x12; 2500]; // ~3 fragmentos
        let pkts = packetize(KIND_VIDEO, 1, true, 9000, &frame_data);
        let viewers = host_mgr.active_viewers();
        eprintln!("Test sending {} pkts to {} viewers", pkts.len(), viewers.len());
        for p in pkts {
            let b = Bytes::copy_from_slice(&p);
            for v in &viewers {
                let res = v.send_datagram(b.clone());
                eprintln!("v.send_datagram result: {res:?}");
            }
        }

        // Verifica se o evento de vídeo chegou no receptor
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
}
