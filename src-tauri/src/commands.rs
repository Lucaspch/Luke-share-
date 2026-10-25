//! Comandos Tauri expostos ao frontend (100% P2P Nativo sobre Iroh).

use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::State;
use uuid::Uuid;

use crate::capture::sources::{list_sources, SourceInfo};
use crate::net::p2p::{create_endpoint, RoomTicket};
use crate::net::protocol::PeerStatus;
use crate::pipeline::receiver::ReceiverPipeline;
use crate::pipeline::sender::SenderPipeline;
use crate::pipeline::StreamEvent;
use crate::video::resolution::Preset;

pub struct LocalIdentity {
    pub id: Uuid,
    nick: Mutex<String>,
    status: Mutex<PeerStatus>,
}

impl LocalIdentity {
    pub fn new(nick: String) -> Arc<Self> {
        Arc::new(Self {
            id: Uuid::new_v4(),
            nick: Mutex::new(nick),
            status: Mutex::new(PeerStatus::Idle),
        })
    }

    pub fn nick(&self) -> String {
        self.nick.lock().clone()
    }

    pub fn set_nick(&self, n: String) {
        *self.nick.lock() = n;
    }

    pub fn status(&self) -> PeerStatus {
        *self.status.lock()
    }

    pub fn set_status(&self, s: PeerStatus) {
        *self.status.lock() = s;
    }
}

pub struct AppState {
    pub local_peer: Arc<LocalIdentity>,
    pub active_sender: Mutex<Option<SenderPipeline>>,
    pub active_receivers: Mutex<std::collections::HashMap<String, ReceiverPipeline>>,
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
}

#[derive(Serialize)]
pub struct ShareInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    pub room_ticket: Option<String>,
}

#[tauri::command]
pub fn get_my_info(state: State<'_, AppState>) -> Result<MyInfo, String> {
    Ok(MyInfo {
        id: state.local_peer.id,
        nick: state.local_peer.nick(),
        status: state.local_peer.status(),
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

    // Seamless Switch: Se já houver um Sender ativo, alternamos a fonte sem reiniciar a sala P2P
    {
        let mut active_lock = state.active_sender.lock();
        if let Some(ref mut sender) = *active_lock {
            tracing::info!("commands: Alternando fonte de transmissão no Sender ativo (Seamless Switch)");
            sender
                .switch_source(
                    source,
                    preset,
                    fps_target,
                    bitrate_kbps,
                    codec,
                    audio_muted,
                )
                .map_err(|e| format!("Erro ao alternar fonte de transmissão: {e:#}"))?;

            let out_res = sender.out_resolution;
            let active_fps = sender.active_fps;
            let active_codec = sender.active_codec.as_str().to_string();
            let room_ticket = sender.room_ticket.as_ref().map(|t| t.raw.clone());

            let _ = on_preview.send(StreamEvent::Config {
                width: out_res.0,
                height: out_res.1,
                fps: active_fps,
                codec: active_codec.clone(),
            });

            sender.request_keyframe();

            return Ok(ShareInfo {
                width: out_res.0,
                height: out_res.1,
                fps: active_fps,
                codec: active_codec,
                room_ticket,
            });
        }
    }

    let prev_chan = on_preview.clone();
    let preview_sink = Arc::new(move |key: bool, pts: u32, data: Vec<u8>| {
        let b64 = crate::pipeline::base64_encode(&data);
        let _ = prev_chan.send(StreamEvent::Video {
            key,
            pts,
            data: b64,
        });
    }) as Arc<dyn Fn(bool, u32, Vec<u8>) + Send + Sync>;

    let p2p_endpoint = create_endpoint()
        .await
        .map_err(|e| format!("Falha ao inicializar endpoint Iroh P2P: {e:#}"))?;

    let sender = SenderPipeline::start(
        source,
        preset,
        fps_target,
        bitrate_kbps,
        codec,
        audio_muted,
        Some(preview_sink),
        p2p_endpoint,
    )
    .map_err(|e| format!("Erro iniciando pipeline de envio: {e:#}"))?;

    let out_res = sender.out_resolution;
    let active_fps = sender.active_fps;
    let active_codec = sender.active_codec.as_str().to_string();
    let room_ticket = sender.room_ticket.as_ref().map(|t| t.raw.clone());

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
        room_ticket,
    })
}

#[tauri::command]
pub fn stop_share(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(mut sender) = state.active_sender.lock().take() {
        sender.stop();
    }
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
    if !trimmed.starts_with("lk-") {
        return Err("Código de sala inválido. Cole um código P2P válido começando com 'lk-'.".into());
    }

    let event_sink = Arc::new(move |ev: StreamEvent| {
        let _ = on_event.send(ev);
    });

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
