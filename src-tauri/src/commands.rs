//! Comandos Tauri expostos ao frontend (v0.5.0: IPC Binário, WASAPI Nativo, Códigos Curtos e Amigos Ed25519).

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;
use uuid::Uuid;

use crate::audio::AudioCaptureMode;
use crate::capture::sources::{list_sources, SourceInfo};
use crate::net::friends::{Friend, FriendsManager, KnockResult, SocialNotification};
use crate::net::p2p::{create_endpoint, RoomTicket};
use crate::net::protocol::PeerStatus;
use crate::net::short_code::{is_short_code, resolve_short_code};
use crate::pipeline::receiver::ReceiverPipeline;
use crate::pipeline::sender::SenderPipeline;
use crate::pipeline::{dispatch_ipc_event, StreamEvent};
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
    pub friends_manager: Arc<FriendsManager>,
    pub active_sender: Arc<Mutex<Option<SenderPipeline>>>,
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
    pub tag: String,
    pub status: PeerStatus,
}

#[derive(Serialize)]
pub struct ShareInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    pub short_code: Option<String>,
    pub room_ticket: Option<String>,
    pub audio_mode: String,
}

#[tauri::command]
pub fn get_my_info(state: State<'_, AppState>) -> Result<MyInfo, String> {
    Ok(MyInfo {
        id: state.local_peer.id,
        nick: state.local_peer.nick(),
        tag: state.friends_manager.tag.clone(),
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
#[allow(clippy::too_many_arguments)]
pub async fn start_share(
    state: State<'_, AppState>,
    source_id: isize,
    preset_str: String,
    fps_target: Option<u32>,
    bitrate_kbps: Option<u32>,
    codec: Option<String>,
    audio_muted: bool,
    audio_mode: Option<String>,
    on_preview: Channel<InvokeResponseBody>,
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

    let parsed_audio_mode = AudioCaptureMode::from_str_opt(audio_mode.as_deref());
    let audio_mode_label = match (&source, parsed_audio_mode) {
        (SourceInfo::Window { .. }, AudioCaptureMode::AppOnly) => "app",
        _ => "system",
    }
    .to_string();

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
                    parsed_audio_mode,
                )
                .map_err(|e| format!("Erro ao alternar fonte de transmissão: {e:#}"))?;

            let out_res = sender.out_resolution;
            let active_fps = sender.active_fps;
            let active_codec = sender.active_codec.as_str().to_string();
            let room_ticket = sender.room_ticket.as_ref().map(|t| t.raw.clone());
            let short_code = sender.short_code.clone();

            dispatch_ipc_event(
                &on_preview,
                StreamEvent::Config {
                    width: out_res.0,
                    height: out_res.1,
                    fps: active_fps,
                    codec: active_codec.clone(),
                },
            );

            sender.request_keyframe();

            return Ok(ShareInfo {
                width: out_res.0,
                height: out_res.1,
                fps: active_fps,
                codec: active_codec,
                short_code,
                room_ticket,
                audio_mode: audio_mode_label,
            });
        }
    }

    let prev_chan = on_preview.clone();
    let preview_sink = Arc::new(move |key: bool, pts: u32, data: Vec<u8>| {
        dispatch_ipc_event(&prev_chan, StreamEvent::VideoBin { key, pts, data });
    }) as Arc<dyn Fn(bool, u32, Vec<u8>) + Send + Sync>;

    let p2p_endpoint = create_endpoint()
        .await
        .map_err(|e| format!("Falha ao inicializar endpoint Iroh P2P: {e:#}"))?;

    // Aguarda brevemente o home relay conectar para que o ticket e o código curto já tragam rota global
    let _ = tokio::time::timeout(Duration::from_millis(1200), p2p_endpoint.online()).await;

    let sender = SenderPipeline::start(
        source,
        preset,
        fps_target,
        bitrate_kbps,
        codec,
        audio_muted,
        parsed_audio_mode,
        Some(preview_sink),
        p2p_endpoint,
    )
    .map_err(|e| format!("Erro iniciando pipeline de envio: {e:#}"))?;

    let out_res = sender.out_resolution;
    let active_fps = sender.active_fps;
    let active_codec = sender.active_codec.as_str().to_string();
    let room_ticket = sender.room_ticket.as_ref().map(|t| t.raw.clone());
    let short_code = sender.short_code.clone();

    dispatch_ipc_event(
        &on_preview,
        StreamEvent::Config {
            width: out_res.0,
            height: out_res.1,
            fps: active_fps,
            codec: active_codec.clone(),
        },
    );

    sender.request_keyframe();

    *state.active_sender.lock() = Some(sender);
    update_local_status(&state);

    Ok(ShareInfo {
        width: out_res.0,
        height: out_res.1,
        fps: active_fps,
        codec: active_codec,
        short_code,
        room_ticket,
        audio_mode: audio_mode_label,
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
    Ok(sender.as_ref().and_then(|s| {
        s.short_code
            .clone()
            .or_else(|| s.room_ticket.as_ref().map(|t| t.raw.clone()))
    }))
}

#[tauri::command]
pub async fn watch_peer(
    state: State<'_, AppState>,
    stream_id: String,
    peer_ip: String,
    on_event: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    let prev_opt = {
        let mut cur = state.active_receivers.lock();
        cur.remove(&stream_id)
    };
    if let Some(mut prev) = prev_opt {
        prev.stop().await;
    }

    let trimmed = peer_ip.trim();
    let (ticket, short_code_origin) = if is_short_code(trimmed) {
        let resolved = resolve_short_code(trimmed)
            .await
            .map_err(|e| format!("Não foi possível localizar o código curto '{trimmed}': {e:#}"))?;
        (resolved, Some(trimmed.to_string()))
    } else if trimmed.starts_with("lk-") {
        (
            RoomTicket {
                raw: trimmed.to_string(),
            },
            None,
        )
    } else {
        return Err("Código inválido. Digite um código curto (LK-XXXX-XXXX) ou cole um ticket completo (lk-...).".into());
    };

    let event_sink = Arc::new(move |ev: StreamEvent| {
        dispatch_ipc_event(&on_event, ev);
    });

    let endpoint = create_endpoint()
        .await
        .map_err(|e| format!("Falha ao inicializar P2P Iroh: {e:#}"))?;

    let receiver = ReceiverPipeline::start_p2p(
        endpoint,
        ticket,
        short_code_origin,
        state.local_peer.id,
        state.local_peer.nick(),
        true,
        event_sink,
    )
    .await
    .map_err(|e| format!("Falha ao conectar via P2P Nativo: {e:#}"))?;

    state.active_receivers.lock().insert(stream_id, receiver);
    update_local_status(&state);
    Ok(())
}

#[tauri::command]
pub fn set_stream_volume(state: State<'_, AppState>, stream_id: String, volume: f32) -> Result<(), String> {
    if let Some(recv) = state.active_receivers.lock().get(&stream_id) {
        recv.set_volume(volume);
    }
    Ok(())
}

#[tauri::command]
pub fn set_stream_muted(state: State<'_, AppState>, stream_id: String, muted: bool) -> Result<(), String> {
    if let Some(recv) = state.active_receivers.lock().get(&stream_id) {
        recv.set_muted(muted);
    }
    Ok(())
}

#[tauri::command]
pub async fn stop_watching(state: State<'_, AppState>, stream_id: Option<String>) -> Result<(), String> {
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
pub fn request_stream_keyframe(state: State<'_, AppState>, stream_id: Option<String>) -> Result<(), String> {
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

// --- Identidade Permanente (LK#TAG), Amigos e Knock P2P ---

#[tauri::command]
pub fn list_friends(state: State<'_, AppState>) -> Result<Vec<Friend>, String> {
    Ok(state.friends_manager.list_friends())
}

#[tauri::command]
pub async fn send_friend_request(state: State<'_, AppState>, target_tag: String) -> Result<String, String> {
    let my_nick = state.local_peer.nick();
    state
        .friends_manager
        .send_friend_request(&target_tag, &my_nick)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn respond_friend_request(
    state: State<'_, AppState>,
    friend_tag: String,
    accept: bool,
) -> Result<(), String> {
    let my_nick = state.local_peer.nick();
    state
        .friends_manager
        .respond_friend_request(&friend_tag, accept, &my_nick)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_friend(state: State<'_, AppState>, friend_tag: String) -> Result<(), String> {
    state
        .friends_manager
        .remove_friend(&friend_tag)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn poll_social_notifications(state: State<'_, AppState>) -> Result<Vec<SocialNotification>, String> {
    Ok(state.friends_manager.drain_notifications())
}

#[tauri::command]
pub async fn knock_friend_stream(state: State<'_, AppState>, friend_tag: String) -> Result<KnockResult, String> {
    let my_nick = state.local_peer.nick();
    state
        .friends_manager
        .knock_friend_stream(&friend_tag, &my_nick, None)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn respond_knock_request(state: State<'_, AppState>, request_id: String, accept: bool) -> Result<(), String> {
    state.friends_manager.respond_to_knock(&request_id, accept);
    Ok(())
}
