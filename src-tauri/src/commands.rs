//! Comandos Tauri expostos ao frontend.

use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::State;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::capture::sources::{list_sources, SourceInfo};
use crate::net::discovery::{LocalPeer, PeerInfo, PeerTable};
use crate::net::p2p::{create_endpoint, RoomTicket};
use crate::net::protocol::{AudioOffer, PeerStatus, VideoOffer, CONTROL_PORT};
use crate::net::session::ControlServer;
use crate::pipeline::receiver::ReceiverPipeline;
use crate::pipeline::sender::SenderPipeline;
use crate::pipeline::StreamEvent;
use crate::video::resolution::Preset;

pub struct AppState {
    pub local_peer: Arc<LocalPeer>,
    pub peer_table: Arc<PeerTable>,
    pub control_server: Arc<ControlServer>,
    pub active_sender: Mutex<Option<SenderPipeline>>,
    pub active_receiver: Mutex<Option<ReceiverPipeline>>,
    pub shutdown_discovery: Arc<Notify>,
}

#[derive(Serialize)]
pub struct MyInfo {
    pub id: Uuid,
    pub nick: String,
    pub status: PeerStatus,
    pub ip: Option<String>,
    pub adapter_name: Option<String>,
}

#[derive(Serialize)]
pub struct ShareInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    pub media_port: u16,
    pub room_ticket: Option<String>,
}

#[tauri::command]
pub fn get_my_info(state: State<'_, AppState>) -> Result<MyInfo, String> {
    Ok(MyInfo {
        id: state.local_peer.id,
        nick: state.local_peer.nick(),
        status: state.local_peer.status(),
        ip: Some("P2P Nativo Iroh".into()),
        adapter_name: Some("Internet Direta (Sem VPN)".into()),
    })
}

#[tauri::command]
pub fn set_my_nick(state: State<'_, AppState>, nick: String) -> Result<(), String> {
    if !nick.trim().is_empty() {
        state.local_peer.set_nick(nick.trim().to_string());
    }
    Ok(())
}

#[tauri::command]
pub fn list_peers(state: State<'_, AppState>) -> Result<Vec<PeerInfo>, String> {
    Ok(state.peer_table.snapshot())
}

#[tauri::command]
pub fn add_manual_peer(state: State<'_, AppState>, ip: String) -> Result<(), String> {
    let ipv4 = Ipv4Addr::from_str(ip.trim()).map_err(|e| format!("IP inválido: {e}"))?;
    state.peer_table.add_manual(ipv4);
    Ok(())
}

#[tauri::command]
pub fn get_sources() -> Result<Vec<SourceInfo>, String> {
    Ok(list_sources())
}

#[tauri::command]
pub fn get_source_snapshot(source_id: isize) -> Result<String, String> {
    crate::capture::sources::capture_source_snapshot(source_id)
        .map_err(|e| format!("Falha ao capturar miniatura: {e:#}"))
}

#[derive(Serialize)]
pub struct StreamStats {
    pub is_sharing: bool,
    pub viewers_count: usize,
    pub viewers: Vec<String>,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub nacks_served: u64,
}

#[tauri::command]
pub fn get_stream_stats(state: State<'_, AppState>) -> Result<StreamStats, String> {
    let (is_sharing, pkts, bytes, nacks, viewers_names) = if let Some(sender) = state.active_sender.lock().as_ref() {
        let (p, b, n) = sender.stats();
        let names = sender.viewers_info();
        (true, p, b, n, names)
    } else {
        (false, 0, 0, 0, Vec::new())
    };
    let count = viewers_names.len();
    Ok(StreamStats {
        is_sharing,
        viewers_count: count,
        viewers: viewers_names,
        packets_sent: pkts,
        bytes_sent: bytes,
        nacks_served: nacks,
    })
}

#[tauri::command]
pub async fn start_share(
    state: State<'_, AppState>,
    source_id: isize,
    preset_str: String,
    fps_target: Option<u32>,
    bitrate_kbps: Option<u32>,
    codec: Option<String>,
    audio_muted: bool,
    on_preview: Channel<StreamEvent>,
) -> Result<ShareInfo, String> {
    // Para compartilhamento anterior se houver
    {
        let mut cur = state.active_sender.lock();
        if let Some(mut prev) = cur.take() {
            prev.stop();
        }
    }
    state.control_server.stop();

    let sources = list_sources();
    let source = sources
        .into_iter()
        .find(|s| s.id() == source_id)
        .ok_or_else(|| "Fonte de captura não encontrada".to_string())?;

    let preset = match preset_str.to_lowercase().as_str() {
        "source" => Preset::Source,
        "720" | "720p" => Preset::P720,
        "480" | "480p" => Preset::P480,
        _ => Preset::P1080,
    };

    let bind_ip = Ipv4Addr::UNSPECIFIED;

    let preview_sink = Arc::new(move |key: bool, pts: u32, data: Vec<u8>| {
        let b64 = crate::pipeline::base64_encode(&data);
        let _ = on_preview.send(StreamEvent::Video {
            key,
            pts,
            data: b64,
        });
    }) as Arc<dyn Fn(bool, u32, Vec<u8>) + Send + Sync>;

    let p2p_endpoint = match create_endpoint().await {
        Ok(ep) => Some(ep),
        Err(e) => {
            tracing::warn!("commands: falha ao inicializar endpoint Iroh P2P: {e:#}");
            None
        }
    };

    let sender = SenderPipeline::start(
        bind_ip,
        source.clone(),
        preset,
        fps_target,
        bitrate_kbps,
        codec,
        audio_muted,
        state.control_server.clone(),
        Some(preview_sink),
        p2p_endpoint,
    )
    .map_err(|e| format!("Erro iniciando pipeline de envio: {e:#}"))?;

    let out_res = sender.out_resolution;
    let active_fps = sender.active_fps;
    let active_codec = sender.active_codec.as_str().to_string();
    let media_port = sender.local_media_port;
    let room_ticket = sender.room_ticket.as_ref().map(|t| t.raw.clone());

    // Inicia servidor de controle TCP no IP do Radmin / LAN para compatibilidade
    let v_offer = VideoOffer {
        width: out_res.0,
        height: out_res.1,
        fps: active_fps,
        codec: active_codec.clone(),
    };
    let a_offer = Some(AudioOffer {
        sample_rate: 48000,
        channels: 2,
    });

    let key_req = sender.keyframe_requester();
    let on_watch = Arc::new(move |_id, _nick, _ip, _port| {
        key_req();
        true
    });

    state
        .control_server
        .start(
            bind_ip,
            CONTROL_PORT,
            v_offer,
            a_offer,
            media_port,
            on_watch,
            Arc::new(|_id| {}),
        )
        .await
        .map_err(|e| format!("Erro iniciando servidor de controle TCP: {e:#}"))?;

    *state.active_sender.lock() = Some(sender);
    state.local_peer.set_status(PeerStatus::Streaming);

    Ok(ShareInfo {
        width: out_res.0,
        height: out_res.1,
        fps: active_fps,
        codec: active_codec,
        media_port,
        room_ticket,
    })
}

#[tauri::command]
pub fn stop_share(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(mut sender) = state.active_sender.lock().take() {
        sender.stop();
    }
    state.control_server.stop();
    state.local_peer.set_status(PeerStatus::Idle);
    Ok(())
}

#[tauri::command]
pub fn set_audio_muted(state: State<'_, AppState>, muted: bool) -> Result<(), String> {
    if let Some(sender) = state.active_sender.lock().as_ref() {
        sender.set_audio_muted(muted);
    }
    Ok(())
}

#[tauri::command]
pub fn get_room_ticket(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let sender = state.active_sender.lock();
    Ok(sender.as_ref().and_then(|s| s.room_ticket.as_ref().map(|t| t.raw.clone())))
}

#[tauri::command]
pub async fn watch_peer(
    state: State<'_, AppState>,
    peer_ip: String,
    on_event: Channel<StreamEvent>,
) -> Result<(), String> {
    // Encerra qualquer visualização anterior (solta o lock antes do await)
    let prev_opt = {
        let mut cur = state.active_receiver.lock();
        cur.take()
    };
    if let Some(mut prev) = prev_opt {
        prev.stop().await;
    }

    let trimmed = peer_ip.trim();
    let event_sink = Arc::new(move |ev: StreamEvent| {
        let _ = on_event.send(ev);
    });

    // 1. Conexão P2P Nativa sobre Iroh (quando fornecido Ticket lk-...)
    if trimmed.starts_with("lk-") {
        let ticket = RoomTicket {
            raw: trimmed.to_string(),
        };
        let endpoint = create_endpoint()
            .await
            .map_err(|e| format!("Falha ao inicializar P2P Iroh: {e:#}"))?;

        let receiver = ReceiverPipeline::start_p2p(
            endpoint,
            ticket,
            state.local_peer.id,
            state.local_peer.nick(),
            event_sink,
        )
        .await
        .map_err(|e| format!("Falha ao conectar via P2P Nativo: {e:#}"))?;

        *state.active_receiver.lock() = Some(receiver);
        state.local_peer.set_status(PeerStatus::Watching);
        return Ok(());
    }

    // 2. Conexão direta por IP (LAN)
    let remote_ip = Ipv4Addr::from_str(trimmed).map_err(|e| format!("Endereço IP ou Ticket de sala inválido: {e}"))?;
    let bind_ip = Ipv4Addr::UNSPECIFIED;

    let receiver = ReceiverPipeline::start(
        bind_ip,
        remote_ip,
        CONTROL_PORT,
        state.local_peer.id,
        state.local_peer.nick(),
        event_sink,
    )
    .await
    .map_err(|e| format!("Falha ao conectar com o anfitrião: {e:#}"))?;

    *state.active_receiver.lock() = Some(receiver);
    state.local_peer.set_status(PeerStatus::Watching);
    Ok(())
}

#[tauri::command]
pub async fn stop_watching(state: State<'_, AppState>) -> Result<(), String> {
    // Solta o lock do Mutex antes de chamar await
    let receiver_opt = {
        let mut cur = state.active_receiver.lock();
        cur.take()
    };
    if let Some(mut receiver) = receiver_opt {
        receiver.stop().await;
    }
    state.local_peer.set_status(PeerStatus::Idle);
    Ok(())
}

#[tauri::command]
pub fn request_stream_keyframe(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(recv) = state.active_receiver.lock().as_ref() {
        recv.request_keyframe();
    }
    Ok(())
}
