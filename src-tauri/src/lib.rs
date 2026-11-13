pub mod audio;
pub mod capture;
pub mod commands;
pub mod net;
pub mod pipeline;
pub mod updater;
pub mod video;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use capture::StreamKind;
use commands::*;
use net::friends::FriendsManager;
use net::groups::{GetActiveTrackTicketsFn, GroupManager};
use pipeline::sender::SenderPipeline;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt().try_init();

    let default_nick = commands::load_saved_nick()
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "Luke-User".into());

    let local_peer = LocalIdentity::new(default_nick);
    let friends_manager = FriendsManager::load_or_create().expect("falha ao inicializar identidade criptográfica Ed25519");
    let group_manager = GroupManager::load_or_create(friends_manager.signing_key_clone())
        .expect("falha ao inicializar gerenciador de grupos");
    let active_senders: Arc<Mutex<HashMap<StreamKind, SenderPipeline>>> = Arc::new(Mutex::new(HashMap::new()));

    let fm_boot = friends_manager.clone();
    let gm_boot = group_manager.clone();
    let lp_boot = local_peer.clone();
    let senders_boot = active_senders.clone();
    let senders_tracks_boot = active_senders.clone();

    let app_state = AppState {
        local_peer,
        friends_manager,
        group_manager,
        active_senders,
        active_receivers: Mutex::new(HashMap::new()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .setup(move |_app| {
            let get_nick = Arc::new(move || lp_boot.nick());
            let get_ticket = Arc::new(move || {
                let lock = senders_boot.lock();
                lock.get(&StreamKind::Screen)
                    .or_else(|| lock.get(&StreamKind::Camera))
                    .and_then(|s| s.room_ticket.as_ref().map(|t| t.raw.clone()))
            });
            let get_tracks: GetActiveTrackTicketsFn = Arc::new(move || {
                let lock = senders_tracks_boot.lock();
                let scr = lock
                    .get(&StreamKind::Screen)
                    .and_then(|s| s.room_ticket.as_ref().map(|t| t.raw.clone()));
                let cam = lock
                    .get(&StreamKind::Camera)
                    .and_then(|s| s.room_ticket.as_ref().map(|t| t.raw.clone()));
                (scr, cam)
            });
            fm_boot.start_daemons(get_nick, get_ticket, Some(gm_boot), Some(get_tracks));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_my_info,
            set_my_nick,
            get_sources,
            get_source_snapshot,
            start_share,
            stop_share,
            list_cameras,
            start_camera_share,
            stop_camera_share,
            set_audio_muted,
            watch_peer,
            set_stream_volume,
            set_stream_muted,
            stop_watching,
            get_stream_stats,
            get_room_ticket,
            request_stream_keyframe,
            list_friends,
            send_friend_request,
            respond_friend_request,
            remove_friend,
            poll_social_notifications,
            knock_friend_stream,
            respond_knock_request,
            get_groups_state,
            create_group,
            create_group_room,
            select_group,
            enter_group_room,
            exit_group_room,
            leave_group,
            create_group_invite,
            join_group_by_code,
            invite_friend_to_group,
            respond_group_invite,
            updater::check_for_updates,
            updater::download_and_install_update,
            updater::restart_app,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao executar o Luke-Share");
}
