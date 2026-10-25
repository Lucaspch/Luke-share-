pub mod audio;
pub mod capture;
pub mod commands;
pub mod net;
pub mod pipeline;
pub mod updater;
pub mod video;

use std::sync::Arc;

use parking_lot::Mutex;

use commands::*;
use net::friends::FriendsManager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt().try_init();

    let default_nick = commands::load_saved_nick()
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "Luke-User".into());

    let local_peer = LocalIdentity::new(default_nick);
    let friends_manager = FriendsManager::load_or_create().expect("falha ao inicializar identidade criptográfica Ed25519");
    let active_sender: Arc<Mutex<Option<pipeline::sender::SenderPipeline>>> = Arc::new(Mutex::new(None));

    let fm_boot = friends_manager.clone();
    let lp_boot = local_peer.clone();
    let sender_boot = active_sender.clone();

    let app_state = AppState {
        local_peer,
        friends_manager,
        active_sender,
        active_receivers: Mutex::new(std::collections::HashMap::new()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .setup(move |_app| {
            let get_nick = Arc::new(move || lp_boot.nick());
            let get_ticket = Arc::new(move || {
                sender_boot
                    .lock()
                    .as_ref()
                    .and_then(|s| s.room_ticket.as_ref().map(|t| t.raw.clone()))
            });
            fm_boot.start_daemons(get_nick, get_ticket);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_my_info,
            set_my_nick,
            get_sources,
            get_source_snapshot,
            start_share,
            stop_share,
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
            updater::check_for_updates,
            updater::download_and_install_update,
            updater::restart_app,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao executar o Luke-Share");
}
