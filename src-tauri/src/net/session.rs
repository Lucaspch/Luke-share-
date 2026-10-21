//! Gerenciamento de sessões TCP de controle (Sharer ⇄ Viewers).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

use super::protocol::*;

/// Representa um espectador conectado ao transmissor.
pub struct ViewerSession {
    pub peer_id: Uuid,
    pub nick: String,
    pub ip: Ipv4Addr,
    pub media_port: u16,
    tx_ctrl: mpsc::Sender<Control>,
}

impl ViewerSession {
    pub fn media_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(self.ip, self.media_port))
    }

    pub async fn send(&self, msg: Control) -> Result<()> {
        self.tx_ctrl.send(msg).await.map_err(|_| anyhow!("sessão do espectador desconectada"))
    }
}

pub type WatchRequestCallback = Arc<dyn Fn(Uuid, String, Ipv4Addr, u16) -> bool + Send + Sync>;

/// Servidor de controle do transmissor.
pub struct ControlServer {
    viewers: Arc<Mutex<HashMap<Uuid, ViewerSession>>>,
    shutdown: Arc<Notify>,
}

impl ControlServer {
    pub fn new() -> Self {
        Self {
            viewers: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(Notify::new()),
        }
    }

    pub fn viewers(&self) -> Vec<ViewerSession> {
        let m = self.viewers.lock();
        m.values()
            .map(|v| ViewerSession {
                peer_id: v.peer_id,
                nick: v.nick.clone(),
                ip: v.ip,
                media_port: v.media_port,
                tx_ctrl: v.tx_ctrl.clone(),
            })
            .collect()
    }

    pub fn viewer_media_addrs(&self) -> Vec<SocketAddr> {
        let m = self.viewers.lock();
        m.values().map(|v| v.media_addr()).collect()
    }

    pub fn viewers_count(&self) -> usize {
        self.viewers.lock().len()
    }

    pub async fn broadcast(&self, msg: Control) {
        let list = self.viewers();
        for v in list {
            let _ = v.send(msg.clone()).await;
        }
    }

    pub async fn broadcast_viewers_map(viewers: &Arc<Mutex<HashMap<Uuid, ViewerSession>>>) {
        let list: Vec<String> = {
            let m = viewers.lock();
            m.values().map(|v| v.nick.clone()).collect()
        };
        let count = list.len() as u32;
        let msg = Control::ViewersUpdate { count, viewers: list };
        let sessions: Vec<ViewerSession> = {
            let m = viewers.lock();
            m.values()
                .map(|v| ViewerSession {
                    peer_id: v.peer_id,
                    nick: v.nick.clone(),
                    ip: v.ip,
                    media_port: v.media_port,
                    tx_ctrl: v.tx_ctrl.clone(),
                })
                .collect()
        };
        for s in sessions {
            let _ = s.send(msg.clone()).await;
        }
    }

    pub fn stop(&self) {
        self.shutdown.notify_waiters();
        self.viewers.lock().clear();
    }

    /// Inicia o listener de controle no IP do Radmin.
    pub async fn start(
        &self,
        bind_ip: Ipv4Addr,
        port: u16,
        offer_video: VideoOffer,
        offer_audio: Option<AudioOffer>,
        media_port: u16,
        on_request: WatchRequestCallback,
        on_viewer_left: Arc<dyn Fn(Uuid) + Send + Sync>,
    ) -> Result<()> {
        let listener = TcpListener::bind(SocketAddr::V4(SocketAddrV4::new(bind_ip, port))).await?;
        let viewers = self.viewers.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.notified() => break,
                    accept_res = listener.accept() => {
                        let Ok((stream, addr)) = accept_res else { continue };
                        let SocketAddr::V4(addr_v4) = addr else { continue };
                        let peer_ip = *addr_v4.ip();
                        let viewers = viewers.clone();
                        let on_request = on_request.clone();
                        let on_viewer_left = on_viewer_left.clone();
                        let v_offer = offer_video.clone();
                        let a_offer = offer_audio.clone();

                        tokio::spawn(async move {
                            let (mut rd, mut wr) = stream.into_split();
                            let Ok(first_msg) = read_control(&mut rd).await else { return };
                            let (peer_id, nick, client_udp_port) = match first_msg {
                                Control::Watch { peer_id, nick, udp_port } => (peer_id, nick, udp_port),
                                _ => return,
                            };

                            let allowed = on_request(peer_id, nick.clone(), peer_ip, client_udp_port);
                            if !allowed {
                                let _ = write_control(&mut wr, &Control::Reject { reason: "Conexão recusada pelo anfitrião".into() }).await;
                                return;
                            }

                            let accept = Control::Accept {
                                video: v_offer,
                                audio: a_offer,
                                udp_port: media_port,
                            };
                            if write_control(&mut wr, &accept).await.is_err() {
                                return;
                            }

                            let (tx_ctrl, mut rx_ctrl) = mpsc::channel::<Control>(16);
                            let session = ViewerSession {
                                peer_id,
                                nick,
                                ip: peer_ip,
                                media_port: client_udp_port,
                                tx_ctrl,
                            };
                            viewers.lock().insert(peer_id, session);
                            Self::broadcast_viewers_map(&viewers).await;

                            // Loop de escrita para o viewer
                            let write_task = tokio::spawn(async move {
                                while let Some(msg) = rx_ctrl.recv().await {
                                    if write_control(&mut wr, &msg).await.is_err() {
                                        break;
                                    }
                                }
                            });

                            // Loop de leitura (aguarda Bye ou desconexão)
                            loop {
                                match read_control(&mut rd).await {
                                    Ok(Control::Bye) | Err(_) => break,
                                    _ => {}
                                }
                            }

                            write_task.abort();
                            viewers.lock().remove(&peer_id);
                            Self::broadcast_viewers_map(&viewers).await;
                            on_viewer_left(peer_id);
                        });
                    }
                }
            }
        });

        Ok(())
    }
}

/// Cliente de controle usado pelo espectador para se conectar ao transmissor.
pub struct ViewerClient {
    pub remote_ip: Ipv4Addr,
    pub remote_media_port: u16,
    pub video_offer: VideoOffer,
    pub audio_offer: Option<AudioOffer>,
    tx_bye: Option<mpsc::Sender<()>>,
}

impl ViewerClient {
    pub async fn connect(
        sharer_ip: Ipv4Addr,
        sharer_ctrl_port: u16,
        my_id: Uuid,
        my_nick: String,
        my_media_udp_port: u16,
        on_reconfigure: Arc<dyn Fn(VideoOffer) + Send + Sync>,
        on_audio_state: Arc<dyn Fn(bool) + Send + Sync>,
        on_viewers_update: Arc<dyn Fn(u32, Vec<String>) + Send + Sync>,
    ) -> Result<Self> {
        let addr = SocketAddr::V4(SocketAddrV4::new(sharer_ip, sharer_ctrl_port));
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("tempo esgotado ao conectar ao anfitrião"))?
            .map_err(|e| anyhow!("não foi possível conectar ao anfitrião: {e}"))?;

        let (mut rd, mut wr) = stream.into_split();

        // Envia pedido Watch
        let watch_msg = Control::Watch {
            peer_id: my_id,
            nick: my_nick,
            udp_port: my_media_udp_port,
        };
        write_control(&mut wr, &watch_msg).await?;

        // Aguarda Accept ou Reject
        let resp = tokio::time::timeout(Duration::from_secs(10), read_control(&mut rd))
            .await
            .map_err(|_| anyhow!("tempo esgotado aguardando resposta do anfitrião"))?
            .map_err(|e| anyhow!("erro lendo resposta: {e}"))?;

        let (video_offer, audio_offer, remote_media_port) = match resp {
            Control::Accept { video, audio, udp_port } => (video, audio, udp_port),
            Control::Reject { reason } => return Err(anyhow!("Conexão rejeitada: {reason}")),
            _ => return Err(anyhow!("Resposta inesperada do anfitrião")),
        };

        let (tx_bye, mut rx_bye) = mpsc::channel::<()>(1);

        // Tarefa em segundo plano para manter conexão e escutar mensagens de controle
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx_bye.recv() => {
                        let _ = write_control(&mut wr, &Control::Bye).await;
                        break;
                    }
                    ctrl = read_control(&mut rd) => {
                        match ctrl {
                            Ok(Control::Reconfigure { video }) => on_reconfigure(video),
                            Ok(Control::AudioState { enabled }) => on_audio_state(enabled),
                            Ok(Control::ViewersUpdate { count, viewers }) => on_viewers_update(count, viewers),
                            Ok(Control::Bye) | Err(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Self {
            remote_ip: sharer_ip,
            remote_media_port,
            video_offer,
            audio_offer,
            tx_bye: Some(tx_bye),
        })
    }

    pub async fn close(&mut self) {
        if let Some(tx) = self.tx_bye.take() {
            let _ = tx.send(()).await;
        }
    }
}
