pub mod audio;
pub mod capture;
pub mod commands;
pub mod net;
pub mod pipeline;
pub mod updater;
pub mod video;

use std::net::Ipv4Addr;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

use commands::*;
use net::discovery::{DiscoveryConfig, LocalPeer, PeerTable};
use net::protocol::{CONTROL_PORT, DISCOVERY_PORT};
use net::session::ControlServer;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt().try_init();

    let bind_ip = Ipv4Addr::UNSPECIFIED;
    let broadcast = Some(Ipv4Addr::BROADCAST);

    let default_nick = commands::load_saved_nick()
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "Luke-User".into());

    let local_peer = LocalPeer::new(default_nick, CONTROL_PORT);
    let peer_table = PeerTable::new();
    let shutdown_discovery = Arc::new(Notify::new());
    let control_server = Arc::new(ControlServer::new());

    // Inicia daemon de descoberta em segundo plano (LAN e loopback)
    let d_cfg = DiscoveryConfig {
        bind_ip,
        port: DISCOVERY_PORT,
        broadcast,
        extra_targets: vec![],
        unicast_port: DISCOVERY_PORT,
    };

    let (m, t, s) = (
        local_peer.clone(),
        peer_table.clone(),
        shutdown_discovery.clone(),
    );
    tauri::async_runtime::spawn(async move {
        let _ = net::discovery::run(d_cfg, m, t, Arc::new(|_| {}), s).await;
    });

    let friends_manager = Arc::new(
        net::friends::FriendsManager::load_or_create()
            .expect("falha ao inicializar gerenciador de identidade e amigos"),
    );

    let app_state = AppState {
        local_peer,
        peer_table,
        control_server,
        friends_manager,
        active_sender: Mutex::new(None),
        active_receivers: Mutex::new(std::collections::HashMap::new()),
        shutdown_discovery,
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_my_info,
            set_my_nick,
            list_peers,
            add_manual_peer,
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
            list_friends,
            send_friend_request,
            respond_friend_request,
            remove_friend,
            knock_friend_stream,
            sync_friends_inbox,
            updater::check_for_updates,
            updater::download_and_install_update,
            updater::restart_app,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao executar o Luke-Share");
}
