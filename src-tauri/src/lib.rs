pub mod audio;
pub mod capture;
pub mod commands;
pub mod net;
pub mod pipeline;
pub mod updater;
pub mod video;

use parking_lot::Mutex;

use commands::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt().try_init();

    let default_nick = commands::load_saved_nick()
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "Luke-User".into());

    let local_peer = LocalIdentity::new(default_nick);

    let app_state = AppState {
        local_peer,
        active_sender: Mutex::new(None),
        active_receivers: Mutex::new(std::collections::HashMap::new()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_my_info,
            set_my_nick,
            get_sources,
            get_source_snapshot,
            start_share,
            stop_share,
            set_audio_muted,
            watch_peer,
            stop_watching,
            get_stream_stats,
            get_room_ticket,
            request_stream_keyframe,
            updater::check_for_updates,
            updater::download_and_install_update,
            updater::restart_app,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao executar o Luke-Share");
}
