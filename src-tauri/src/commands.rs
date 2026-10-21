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
    pub active_receivers: Mutex<std::collections::HashMap<String, ReceiverPipeline>>,
    pub shutdown_discovery: Arc<Notify>,
}

fn update_local_status(state: &AppState) {
    let is_streaming = state.active_sender.lock().is_some();
    let is_watching = !state.active_receivers.lock().is_empty();
    let status = if is_streaming {
        PeerStatus::Streaming
    } else if is_watching {
        PeerStatus::Watching
    } else {
        PeerStatus::Idle
    };
    state.local_peer.set_status(status);
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

fn get_saved_nick_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("LOCALAPPDATA"))
        .ok()?;
    let mut dir = std::path::PathBuf::from(base);
    dir.push("Luke-Share");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("nick.txt");
    Some(dir)
}

pub fn load_saved_nick() -> Option<String> {
    let path = get_saved_nick_path()?;
    if let Ok(content) = std::fs::read_to_string(path) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

pub fn save_nick(nick: &str) {
    if let Some(path) = get_saved_nick_path() {
        let _ = std::fs::write(path, nick.trim());
    }
}

#[tauri::command]
pub fn set_my_nick(state: State<'_, AppState>, nick: String) -> Result<(), String> {
    let trimmed = nick.trim();
    if !trimmed.is_empty() {
        state.local_peer.set_nick(trimmed.to_string());
        save_nick(trimmed);
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

    let prev_chan = on_preview.clone();
    let preview_sink = Arc::new(move |key: bool, pts: u32, data: Vec<u8>| {
        let b64 = crate::pipeline::base64_encode(&data);
        let _ = prev_chan.send(StreamEvent::Video {
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

    // Envia evento de configuração inicial para a prévia local
    let _ = on_preview.send(StreamEvent::Config {
        width: out_res.0,
        height: out_res.1,
        fps: active_fps,
        codec: active_codec.clone(),
    });

    // Solicita keyframe IDR imediato para que a prévia e espectadores iniciem sem atraso
    sender.request_keyframe();

    *state.active_sender.lock() = Some(sender);
    update_local_status(&state);

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
    update_local_status(&state);
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
    stream_id: String,
    peer_ip: String,
    on_event: Channel<StreamEvent>,
) -> Result<(), String> {
    // Se já houver um receiver com esse stream_id, encerra ele primeiro
    let prev_opt = {
        let mut cur = state.active_receivers.lock();
        cur.remove(&stream_id)
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

        state.active_receivers.lock().insert(stream_id, receiver);
        update_local_status(&state);
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

    state.active_receivers.lock().insert(stream_id, receiver);
    update_local_status(&state);
    Ok(())
}

#[tauri::command]
pub async fn stop_watching(
    state: State<'_, AppState>,
    stream_id: Option<String>,
) -> Result<(), String> {
    let to_stop: Vec<ReceiverPipeline> = {
        let mut map = state.active_receivers.lock();
        if let Some(ref id) = stream_id {
            map.remove(id).into_iter().collect()
        } else {
            map.drain().map(|(_, r)| r).collect()
        }
    };

    for mut r in to_stop {
        r.stop().await;
    }

    update_local_status(&state);
    Ok(())
}

#[tauri::command]
pub fn request_stream_keyframe(
    state: State<'_, AppState>,
    stream_id: Option<String>,
) -> Result<(), String> {
    let receivers = state.active_receivers.lock();
    if let Some(ref id) = stream_id {
        if let Some(recv) = receivers.get(id) {
            recv.request_keyframe();
        }
    } else {
        for recv in receivers.values() {
            recv.request_keyframe();
        }
    }
    Ok(())
}
