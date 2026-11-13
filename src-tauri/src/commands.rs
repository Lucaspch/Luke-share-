//! Comandos Tauri expostos ao frontend (v1.0.0: Grupos P2P estilo Discord sem voz, Webcam nativa Media Foundation, IPC Binário e Privado 1-on-1).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;
use uuid::Uuid;

use crate::audio::AudioCaptureMode;
use crate::capture::camera::{list_cameras as enumerate_cameras, CameraDeviceInfo};
use crate::capture::sources::{list_sources, SourceInfo};
use crate::capture::{CaptureTarget, StreamKind};
use crate::net::friends::{Friend, FriendsManager, KnockResult, SocialNotification};
use crate::net::groups::{
    GroupInviteBundle, GroupManager, GroupRoom, GroupView, GroupsStateSnapshot,
};
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
    pub group_manager: Arc<GroupManager>,
    pub active_senders: Arc<Mutex<HashMap<StreamKind, SenderPipeline>>>,
    pub active_receivers: Mutex<HashMap<String, ReceiverPipeline>>,
}

fn update_local_status(state: &AppState) {
    let is_streaming = !state.active_senders.lock().is_empty();
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

fn notify_active_group_tracks(state: &AppState) {
    let active_gid = state
        .group_manager
        .snapshot(&state.local_peer.nick())
        .active_group_id;
    if let Some(gid) = active_gid {
        state.group_manager.trigger_immediate_sync(&gid);
    }
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
    pub is_sharing_screen: bool,
    pub is_sharing_camera: bool,
    pub viewers_count: usize,
    pub viewers: Vec<String>,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub nacks_served: u64,
}

#[tauri::command]
pub fn get_stream_stats(state: State<'_, AppState>) -> Result<StreamStats, String> {
    let senders = state.active_senders.lock();
    let has_screen = senders.contains_key(&StreamKind::Screen);
    let has_camera = senders.contains_key(&StreamKind::Camera);
    let is_sharing = has_screen || has_camera;

    let mut pkts = 0u64;
    let mut bytes = 0u64;
    let mut nacks = 0u64;
    let mut viewers_names: Vec<String> = Vec::new();
    for sender in senders.values() {
        let (p, b, n) = sender.stats();
        pkts += p;
        bytes += b;
        nacks += n;
        for name in sender.viewers_info() {
            if !viewers_names.contains(&name) {
                viewers_names.push(name);
            }
        }
    }

    let count = viewers_names.len();
    Ok(StreamStats {
        is_sharing,
        is_sharing_screen: has_screen,
        is_sharing_camera: has_camera,
        viewers_count: count,
        viewers: viewers_names,
        packets_sent: pkts,
        bytes_sent: bytes,
        nacks_served: nacks,
    })
}

fn parse_preset(preset_str: &str) -> Preset {
    match preset_str.to_lowercase().as_str() {
        "source" => Preset::Source,
        "720" | "720p" => Preset::P720,
        "480" | "480p" => Preset::P480,
        _ => Preset::P1080,
    }
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
    publish_short_code: Option<bool>,
    on_preview: Channel<InvokeResponseBody>,
) -> Result<ShareInfo, String> {
    let sources = list_sources();
    let source = sources
        .into_iter()
        .find(|s| s.id() == source_id)
        .ok_or_else(|| "Fonte de captura não encontrada".to_string())?;

    let preset = parse_preset(&preset_str);

    let parsed_audio_mode = AudioCaptureMode::from_str_opt(audio_mode.as_deref());
    let audio_mode_label = match (&source, parsed_audio_mode) {
        (SourceInfo::Window { .. }, AudioCaptureMode::AppOnly) => "app",
        _ => "system",
    }
    .to_string();

    // Seamless Switch: Se já houver uma Tela ativa, alternamos a fonte sem reiniciar a sala P2P
    {
        let mut active_lock = state.active_senders.lock();
        if let Some(sender) = active_lock.get_mut(&StreamKind::Screen) {
            tracing::info!("commands: Alternando fonte de tela no Sender ativo (Seamless Switch)");
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

    let sender = SenderPipeline::start_target(
        CaptureTarget::Screen(source),
        preset,
        fps_target,
        bitrate_kbps,
        codec,
        audio_muted,
        parsed_audio_mode,
        publish_short_code.unwrap_or(true),
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

    state.active_senders.lock().insert(StreamKind::Screen, sender);
    update_local_status(&state);
    notify_active_group_tracks(&state);

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
    if let Some(mut sender) = state.active_senders.lock().remove(&StreamKind::Screen) {
        sender.stop();
    }
    update_local_status(&state);
    notify_active_group_tracks(&state);
    Ok(())
}

#[tauri::command]
pub fn list_cameras() -> Result<Vec<CameraDeviceInfo>, String> {
    Ok(enumerate_cameras())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn start_camera_share(
    state: State<'_, AppState>,
    device_id: String,
    preset_str: Option<String>,
    fps_target: Option<u32>,
    bitrate_kbps: Option<u32>,
    codec: Option<String>,
    publish_short_code: Option<bool>,
    on_preview: Channel<InvokeResponseBody>,
) -> Result<ShareInfo, String> {
    let cams = enumerate_cameras();
    let cam = cams
        .into_iter()
        .find(|c| c.id == device_id)
        .ok_or_else(|| "Webcam selecionada não está mais conectada".to_string())?;

    if let Some(mut old_cam) = state.active_senders.lock().remove(&StreamKind::Camera) {
        old_cam.stop();
    }

    let preset = parse_preset(preset_str.as_deref().unwrap_or("720p"));
    let prev_chan = on_preview.clone();
    let preview_sink = Arc::new(move |key: bool, pts: u32, data: Vec<u8>| {
        dispatch_ipc_event(&prev_chan, StreamEvent::VideoBin { key, pts, data });
    }) as Arc<dyn Fn(bool, u32, Vec<u8>) + Send + Sync>;

    let p2p_endpoint = create_endpoint()
        .await
        .map_err(|e| format!("Falha ao inicializar endpoint Iroh P2P para webcam: {e:#}"))?;
    let _ = tokio::time::timeout(Duration::from_millis(1200), p2p_endpoint.online()).await;

    let sender = SenderPipeline::start_target(
        CaptureTarget::Camera(cam),
        preset,
        Some(fps_target.unwrap_or(30)),
        Some(bitrate_kbps.unwrap_or(2800)),
        codec,
        true,
        AudioCaptureMode::SystemNoDiscord,
        publish_short_code.unwrap_or(false),
        Some(preview_sink),
        p2p_endpoint,
    )
    .map_err(|e| format!("Erro iniciando webcam P2P: {e:#}"))?;

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
    state.active_senders.lock().insert(StreamKind::Camera, sender);
    update_local_status(&state);
    notify_active_group_tracks(&state);

    Ok(ShareInfo {
        width: out_res.0,
        height: out_res.1,
        fps: active_fps,
        codec: active_codec,
        short_code,
        room_ticket,
        audio_mode: "none".to_string(),
    })
}

#[tauri::command]
pub fn stop_camera_share(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(mut sender) = state.active_senders.lock().remove(&StreamKind::Camera) {
        sender.stop();
    }
    update_local_status(&state);
    notify_active_group_tracks(&state);
    Ok(())
}

#[tauri::command]
pub fn set_audio_muted(state: State<'_, AppState>, muted: bool) -> Result<(), String> {
    if let Some(sender) = state.active_senders.lock().get(&StreamKind::Screen) {
        sender.set_audio_muted(muted);
    }
    Ok(())
}

#[tauri::command]
pub fn get_room_ticket(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let senders = state.active_senders.lock();
    let sender = senders
        .get(&StreamKind::Screen)
        .or_else(|| senders.get(&StreamKind::Camera));
    Ok(sender.and_then(|s| {
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
    let mut notifs = state.friends_manager.drain_notifications();
    for (message, icon) in state.group_manager.drain_toasts() {
        notifs.push(SocialNotification::Toast { message, icon });
    }
    Ok(notifs)
}

// --- Grupos Persistentes P2P (estilo Discord, sem voz) ---

#[tauri::command]
pub fn get_groups_state(state: State<'_, AppState>) -> Result<GroupsStateSnapshot, String> {
    let my_nick = state.local_peer.nick();
    Ok(state.group_manager.snapshot(&my_nick))
}

#[tauri::command]
pub fn create_group(state: State<'_, AppState>, name: String) -> Result<GroupView, String> {
    let my_nick = state.local_peer.nick();
    state
        .group_manager
        .create_group(&name, &my_nick)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_group_room(state: State<'_, AppState>, group_id: String, name: String) -> Result<GroupRoom, String> {
    state
        .group_manager
        .create_room(&group_id, &name)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn select_group(state: State<'_, AppState>, group_id: Option<String>) -> Result<(), String> {
    state.group_manager.select_group(group_id);
    Ok(())
}

#[tauri::command]
pub fn enter_group_room(state: State<'_, AppState>, group_id: String, room_id: String) -> Result<(), String> {
    state
        .group_manager
        .enter_room(&group_id, &room_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn exit_group_room(state: State<'_, AppState>) -> Result<(), String> {
    state.group_manager.exit_room();
    Ok(())
}

#[tauri::command]
pub fn leave_group(state: State<'_, AppState>, group_id: String) -> Result<(), String> {
    state
        .group_manager
        .leave_group(&group_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn create_group_invite(
    state: State<'_, AppState>,
    group_id: String,
) -> Result<GroupInviteBundle, String> {
    let my_nick = state.local_peer.nick();
    state
        .group_manager
        .create_group_invite(&group_id, &my_nick)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn join_group_by_code(state: State<'_, AppState>, code: String) -> Result<GroupView, String> {
    let my_nick = state.local_peer.nick();
    state
        .group_manager
        .join_group_by_code(&code, &my_nick)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn invite_friend_to_group(
    state: State<'_, AppState>,
    group_id: String,
    friend_tag: String,
) -> Result<(), String> {
    let my_nick = state.local_peer.nick();
    let friend = state
        .friends_manager
        .get_friend(&friend_tag)
        .ok_or_else(|| "Amigo não encontrado na sua lista".to_string())?;
    state
        .group_manager
        .invite_friend_to_group(&group_id, &friend.public_key_b58, friend.last_addr, &my_nick)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn respond_group_invite(
    state: State<'_, AppState>,
    group_id: String,
    accept: bool,
) -> Result<Option<GroupView>, String> {
    let my_nick = state.local_peer.nick();
    state
        .group_manager
        .respond_group_invite(&group_id, accept, &my_nick)
        .map_err(|e| e.to_string())
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
