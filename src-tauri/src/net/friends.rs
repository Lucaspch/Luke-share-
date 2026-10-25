//! Identidade Criptográfica Permanente (`LK#TAG` via Ed25519), Lista de Amigos com Confirmação Mútua
//! e Protocolo "Pedir para Entrar" (Knock P2P) sobre Iroh QUIC + Cloudflare Worker opcional.
//!
//! Garantias de segurança:
//! 1. Chave privada Ed25519 de 256 bits gerada uma vez em `%APPDATA%/Luke-Share/identity.key`.
//! 2. `LK#TAG` derivada matematicamente de `SHA-256(salt || pubkey)` — impossível forjar uma tag
//!    ou roubar amizades editando arquivos locais, pois cada envelope exige assinatura Ed25519
//!    verificada (`verify_strict`) e toda conexão P2P valida o certificado TLS 1.3 (`conn.remote_id()`).
//! 3. Amizade mútua obrigatória: presença ("Ao Vivo") e "Pedir para Entrar" só funcionam quando
//!    ambos os lados confirmaram a amizade (`FriendStatus::Accepted`).
//! 4. "Pedir para Entrar" (Knock) trafega diretamente pelo canal P2P do Iroh: o Host vê um toast
//!    com botões **Permitir / Recusar** e só libera o `RoomTicket` após clicar em Permitir.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use iroh::{endpoint::presets, Endpoint, EndpointAddr, PublicKey, SecretKey};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::short_code::{configured_worker_url, hkdf_expand_32, hkdf_extract, SAFE_ALPHABET};

pub const LUKE_SOCIAL_ALPN: &[u8] = b"luke-share-social/1.0";
pub const LUKE_TAG_ALPN: &[u8] = b"luke-share-tag/1.0";

const LUKE_IDENTITY_SALT: &[u8] = b"LUKE-SHARE-ED25519-IDENTITY-SALT-2026";
const LUKE_TAG_DISCOVERY_IKM: &[u8] = b"LUKE-SHARE-V05-TAG-RENDEZVOUS-IKM-2026";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FriendStatus {
    PendingOut, // Você enviou convite (aguardando aceite do outro lado)
    PendingIn,  // Você recebeu convite (aguardando seu aceite)
    Accepted,   // Confirmado mutuamente por ambos
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Friend {
    pub tag: String,
    pub nick: String,
    pub public_key_b58: String,
    pub status: FriendStatus,
    #[serde(default)]
    pub is_online: bool,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub last_addr: Option<EndpointAddr>,
    #[serde(skip)]
    pub last_seen: Option<Instant>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TagCard {
    pub tag: String,
    pub nick: String,
    pub pubkey_b58: String,
    pub signature_hex: String,
    #[serde(default)]
    pub social_addr: Option<EndpointAddr>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FriendEnvelope {
    pub from_tag: String,
    pub from_nick: String,
    pub from_pubkey_b58: String,
    pub to_tag: String,
    pub action: String, // "friend_request" | "friend_accept" | "friend_reject"
    pub timestamp: u64,
    pub signature_hex: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "w")]
enum SocialWire {
    Envelope {
        env: FriendEnvelope,
        #[serde(default)]
        sender_addr: Option<EndpointAddr>,
    },
    EnvelopeAck {
        ok: bool,
        from_tag: String,
        from_nick: String,
        #[serde(default)]
        auto_accept: Option<FriendEnvelope>,
        #[serde(default)]
        receiver_addr: Option<EndpointAddr>,
    },
    PresencePing {
        from_tag: String,
        from_nick: String,
        is_streaming: bool,
        #[serde(default)]
        sender_addr: Option<EndpointAddr>,
    },
    PresencePong {
        from_tag: String,
        from_nick: String,
        is_streaming: bool,
        #[serde(default)]
        responder_addr: Option<EndpointAddr>,
    },
    KnockRequest {
        request_id: String,
        from_tag: String,
        from_nick: String,
    },
    KnockDecision {
        request_id: String,
        accepted: bool,
        room_ticket: Option<String>,
        host_nick: String,
        reason: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind")]
pub enum SocialNotification {
    Toast {
        message: String,
        icon: String,
    },
    IncomingKnock {
        request_id: String,
        from_tag: String,
        from_nick: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KnockResult {
    pub accepted: bool,
    pub room_ticket: Option<String>,
    pub host_nick: String,
    pub message: String,
}

fn get_data_dir() -> Result<PathBuf> {
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("LOCALAPPDATA"))
        .context("Variável APPDATA não encontrada")?;
    let mut dir = PathBuf::from(base);
    dir.push("Luke-Share");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn derive_tag(pubkey: &VerifyingKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(LUKE_IDENTITY_SALT);
    hasher.update(pubkey.as_bytes());
    let hash = hasher.finalize();

    let mut suffix = String::with_capacity(4);
    for i in 0..4 {
        let idx = (hash[i] as usize) & 31;
        suffix.push(SAFE_ALPHABET[idx] as char);
    }
    format!("LK#{suffix}")
}

pub fn normalize_tag(input: &str) -> Result<String> {
    let clean: String = input
        .trim()
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '#')
        .collect();

    let suffix = if let Some(rest) = clean.strip_prefix("LK#") {
        rest
    } else if let Some(rest) = clean.strip_prefix("LK") {
        rest.strip_prefix('#').unwrap_or(rest)
    } else {
        clean.strip_prefix('#').unwrap_or(&clean)
    };

    if suffix.len() != 4 || !suffix.chars().all(|c| SAFE_ALPHABET.contains(&(c as u8))) {
        anyhow::bail!("Tag inválida. Use o formato LK#XXXX (ex.: LK#7M2Q).");
    }
    Ok(format!("LK#{suffix}"))
}

fn derive_tag_discovery_seed(norm_tag: &str) -> [u8; 32] {
    let prk = hkdf_extract(LUKE_TAG_DISCOVERY_IKM, norm_tag.as_bytes());
    hkdf_expand_32(&prk, b"luke-share/v0.5/tag-discovery-ed25519")
}

async fn write_wire<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &SocialWire) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

async fn read_wire<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<SocialWire> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > 64 * 1024 {
        anyhow::bail!("Mensagem social com tamanho inválido ({len})");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct FriendsManager {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    pub tag: String,
    pub friends: Arc<RwLock<HashMap<String, Friend>>>,
    notifications: Arc<Mutex<VecDeque<SocialNotification>>>,
    pending_knocks: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    social_endpoint: Arc<Mutex<Option<Endpoint>>>,
    tag_endpoint: Arc<Mutex<Option<Endpoint>>>,
    stop: Arc<AtomicBool>,
}

impl FriendsManager {
    pub fn load_or_create() -> Result<Arc<Self>> {
        let dir = get_data_dir()?;
        let key_path = dir.join("identity.key");

        let signing_key = if key_path.exists() {
            let bytes = fs::read(&key_path).context("falha ao ler identity.key")?;
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                SigningKey::from_bytes(&arr)
            } else {
                let mut rng = rand::rng();
                let key = SigningKey::generate(&mut rng);
                let _ = fs::write(&key_path, key.to_bytes());
                key
            }
        } else {
            let mut rng = rand::rng();
            let key = SigningKey::generate(&mut rng);
            let _ = fs::write(&key_path, key.to_bytes());
            key
        };

        let verifying_key = signing_key.verifying_key();
        let tag = derive_tag(&verifying_key);

        let mut friends_map = HashMap::new();
        let friends_path = dir.join("friends.json");
        if friends_path.exists() {
            if let Ok(content) = fs::read_to_string(&friends_path) {
                if let Ok(saved) = serde_json::from_str::<Vec<Friend>>(&content) {
                    for mut f in saved {
                        f.is_online = false;
                        f.is_streaming = false;
                        f.last_seen = None;
                        friends_map.insert(f.tag.clone(), f);
                    }
                }
            }
        }

        Ok(Arc::new(Self {
            signing_key,
            verifying_key,
            tag,
            friends: Arc::new(RwLock::new(friends_map)),
            notifications: Arc::new(Mutex::new(VecDeque::new())),
            pending_knocks: Arc::new(Mutex::new(HashMap::new())),
            social_endpoint: Arc::new(Mutex::new(None)),
            tag_endpoint: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        }))
    }

    #[cfg(test)]
    pub fn ephemeral_for_test() -> Arc<Self> {
        let mut rng = rand::rng();
        let signing_key = SigningKey::generate(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let tag = derive_tag(&verifying_key);
        Arc::new(Self {
            signing_key,
            verifying_key,
            tag,
            friends: Arc::new(RwLock::new(HashMap::new())),
            notifications: Arc::new(Mutex::new(VecDeque::new())),
            pending_knocks: Arc::new(Mutex::new(HashMap::new())),
            social_endpoint: Arc::new(Mutex::new(None)),
            tag_endpoint: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn public_key_b58(&self) -> String {
        bs58::encode(self.verifying_key.as_bytes()).into_string()
    }

    pub fn save_friends(&self) {
        #[cfg(not(test))]
        if let Ok(dir) = get_data_dir() {
            let list: Vec<Friend> = self.friends.read().values().cloned().collect();
            if let Ok(json) = serde_json::to_string_pretty(&list) {
                let _ = fs::write(dir.join("friends.json"), json);
            }
        }
    }

    pub fn list_friends(&self) -> Vec<Friend> {
        let mut list: Vec<Friend> = self
            .friends
            .read()
            .values()
            .map(|f| {
                let mut copy = f.clone();
                if let Some(seen) = f.last_seen {
                    if seen.elapsed() > Duration::from_secs(28) {
                        copy.is_online = false;
                        copy.is_streaming = false;
                    }
                }
                copy
            })
            .collect();

        list.sort_by(|a, b| {
            let rank = |s: FriendStatus| match s {
                FriendStatus::PendingIn => 0,
                FriendStatus::Accepted => 1,
                FriendStatus::PendingOut => 2,
            };
            rank(a.status)
                .cmp(&rank(b.status))
                .then_with(|| b.is_streaming.cmp(&a.is_streaming))
                .then_with(|| b.is_online.cmp(&a.is_online))
                .then_with(|| a.nick.to_lowercase().cmp(&b.nick.to_lowercase()))
        });
        list
    }

    pub fn drain_notifications(&self) -> Vec<SocialNotification> {
        let mut q = self.notifications.lock();
        q.drain(..).collect()
    }

    fn push_toast(&self, message: String, icon: &str) {
        self.notifications.lock().push_back(SocialNotification::Toast {
            message,
            icon: icon.to_string(),
        });
    }

    pub fn build_signed_tag_card(&self, my_nick: &str) -> TagCard {
        let pub_b58 = self.public_key_b58();
        let mut msg = Vec::new();
        msg.extend_from_slice(b"LUKE-TAG-CARD-V1:");
        msg.extend_from_slice(self.tag.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(pub_b58.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(my_nick.as_bytes());

        let sig = self.signing_key.sign(&msg);
        let social_addr = self.social_endpoint.lock().as_ref().map(|ep| ep.addr());
        TagCard {
            tag: self.tag.clone(),
            nick: my_nick.to_string(),
            pubkey_b58: pub_b58,
            signature_hex: hex::encode(sig.to_bytes()),
            social_addr,
        }
    }

    pub fn verify_tag_card(card: &TagCard) -> Result<VerifyingKey> {
        let pk_bytes = bs58::decode(&card.pubkey_b58)
            .into_vec()
            .context("Chave pública Base58 inválida")?;
        let arr: [u8; 32] = pk_bytes
            .try_into()
            .map_err(|_| anyhow!("Chave pública Ed25519 deve ter 32 bytes"))?;
        let vk = VerifyingKey::from_bytes(&arr).context("Chave Ed25519 malformada")?;

        let expected = derive_tag(&vk);
        if expected != card.tag {
            anyhow::bail!("Tag {} não corresponde matematicamente à chave pública!", card.tag);
        }

        let sig_bytes = hex::decode(&card.signature_hex).context("Assinatura hex inválida")?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| anyhow!("Assinatura Ed25519 deve ter 64 bytes"))?;
        let sig = Signature::from_bytes(&sig_arr);

        let mut msg = Vec::new();
        msg.extend_from_slice(b"LUKE-TAG-CARD-V1:");
        msg.extend_from_slice(card.tag.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(card.pubkey_b58.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(card.nick.as_bytes());

        vk.verify_strict(&msg, &sig)
            .map_err(|_| anyhow!("Assinatura criptográfica do TagCard inválida"))?;
        Ok(vk)
    }

    pub fn sign_envelope(&self, to_tag: &str, my_nick: &str, action: &str) -> FriendEnvelope {
        let ts = now_unix_secs();
        let pub_b58 = self.public_key_b58();

        let mut msg = Vec::new();
        msg.extend_from_slice(b"LUKE-ENV-V1:");
        msg.extend_from_slice(self.tag.as_bytes());
        msg.extend_from_slice(b"->");
        msg.extend_from_slice(to_tag.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(action.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(&ts.to_be_bytes());

        let sig = self.signing_key.sign(&msg);
        FriendEnvelope {
            from_tag: self.tag.clone(),
            from_nick: my_nick.to_string(),
            from_pubkey_b58: pub_b58,
            to_tag: to_tag.to_string(),
            action: action.to_string(),
            timestamp: ts,
            signature_hex: hex::encode(sig.to_bytes()),
        }
    }

    /// Valida matematicamente a chave pública, a derivação `LK#TAG` e a assinatura Ed25519 estrita.
    pub fn verify_and_apply_envelope(&self, env: &FriendEnvelope) -> Result<Option<String>> {
        if env.to_tag != self.tag {
            return Ok(None);
        }

        let pk_bytes = bs58::decode(&env.from_pubkey_b58).into_vec()?;
        let pk_arr: [u8; 32] = pk_bytes
            .try_into()
            .map_err(|_| anyhow!("Chave pública inválida"))?;
        let peer_vk = VerifyingKey::from_bytes(&pk_arr)?;

        if derive_tag(&peer_vk) != env.from_tag {
            anyhow::bail!("Tentativa de falsificação bloqueada: tag não pertence à chave pública!");
        }

        let sig_bytes = hex::decode(&env.signature_hex)?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| anyhow!("Assinatura inválida"))?;
        let sig = Signature::from_bytes(&sig_arr);

        let mut msg = Vec::new();
        msg.extend_from_slice(b"LUKE-ENV-V1:");
        msg.extend_from_slice(env.from_tag.as_bytes());
        msg.extend_from_slice(b"->");
        msg.extend_from_slice(env.to_tag.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(env.action.as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(&env.timestamp.to_be_bytes());

        peer_vk
            .verify_strict(&msg, &sig)
            .map_err(|_| anyhow!("Assinatura Ed25519 inválida no envelope"))?;

        let mut toast = None;
        let mut changed = false;

        match env.action.as_str() {
            "friend_request" => {
                let mut lock = self.friends.write();
                if let Some(existing) = lock.get_mut(&env.from_tag) {
                    if existing.status == FriendStatus::PendingOut {
                        existing.status = FriendStatus::Accepted;
                        existing.nick = env.from_nick.clone();
                        existing.public_key_b58 = env.from_pubkey_b58.clone();
                        existing.is_online = true;
                        existing.last_seen = Some(Instant::now());
                        changed = true;
                        toast = Some(format!("Amizade mútua confirmada com {} ({})!", env.from_nick, env.from_tag));
                    } else if existing.status == FriendStatus::PendingIn {
                        existing.nick = env.from_nick.clone();
                        existing.public_key_b58 = env.from_pubkey_b58.clone();
                    } else if existing.status == FriendStatus::Accepted {
                        existing.nick = env.from_nick.clone();
                        existing.public_key_b58 = env.from_pubkey_b58.clone();
                        existing.is_online = true;
                        existing.last_seen = Some(Instant::now());
                    }
                } else {
                    lock.insert(
                        env.from_tag.clone(),
                        Friend {
                            tag: env.from_tag.clone(),
                            nick: env.from_nick.clone(),
                            public_key_b58: env.from_pubkey_b58.clone(),
                            status: FriendStatus::PendingIn,
                            is_online: true,
                            is_streaming: false,
                            last_addr: None,
                            last_seen: Some(Instant::now()),
                        },
                    );
                    changed = true;
                    toast = Some(format!(
                        "{} ({}) enviou um pedido de amizade!",
                        env.from_nick, env.from_tag
                    ));
                }
            }
            "friend_accept" => {
                let mut lock = self.friends.write();
                if let Some(existing) = lock.get_mut(&env.from_tag) {
                    if existing.status == FriendStatus::PendingOut || existing.status == FriendStatus::Accepted {
                        let was_pending = existing.status == FriendStatus::PendingOut;
                        existing.status = FriendStatus::Accepted;
                        existing.nick = env.from_nick.clone();
                        existing.public_key_b58 = env.from_pubkey_b58.clone();
                        existing.is_online = true;
                        existing.last_seen = Some(Instant::now());
                        changed = true;
                        if was_pending {
                            toast = Some(format!(
                                "{} ({}) aceitou seu pedido de amizade!",
                                env.from_nick, env.from_tag
                            ));
                        }
                    }
                }
            }
            "friend_reject" => {
                let mut lock = self.friends.write();
                if lock.remove(&env.from_tag).is_some() {
                    changed = true;
                }
            }
            _ => {}
        }

        if changed {
            self.save_friends();
        }
        if let Some(ref t) = toast {
            self.push_toast(t.clone(), "person_add");
        }
        Ok(toast)
    }

    fn maybe_build_auto_accept(&self, peer_tag: &str, my_nick: &str) -> Option<FriendEnvelope> {
        let is_accepted = {
            let lock = self.friends.read();
            lock.get(peer_tag)
                .map(|f| f.status == FriendStatus::Accepted)
                .unwrap_or(false)
        };
        if is_accepted {
            Some(self.sign_envelope(peer_tag, my_nick, "friend_accept"))
        } else {
            None
        }
    }

    fn update_friend_addr(&self, tag: &str, addr: Option<EndpointAddr>) {
        if let Some(addr) = addr {
            let mut lock = self.friends.write();
            if let Some(f) = lock.get_mut(tag) {
                f.last_addr = Some(addr);
            }
        }
    }

    /// Inicia os daemons P2P de Identidade Social (`LUKE_SOCIAL_ALPN`) e Descoberta de Tag (`LUKE_TAG_ALPN`).
    pub fn start_daemons(
        self: &Arc<Self>,
        get_nick: Arc<dyn Fn() -> String + Send + Sync>,
        get_active_ticket: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    ) {
        let mgr = self.clone();
        let nick_fn = get_nick.clone();
        let ticket_fn = get_active_ticket.clone();

        tauri::async_runtime::spawn(async move {
            // 1. Inicializa simultaneamente Endpoint Social (`identity.key`) e Endpoint de Tag (`LK#XXXX`)
            let secret = SecretKey::from_bytes(&mgr.signing_key.to_bytes());
            let tag_seed = derive_tag_discovery_seed(&mgr.tag);

            let (social_res, tag_res) = tokio::join!(
                Endpoint::builder(presets::N0)
                    .secret_key(secret)
                    .alpns(vec![LUKE_SOCIAL_ALPN.to_vec()])
                    .bind(),
                Endpoint::builder(presets::N0)
                    .secret_key(SecretKey::from_bytes(&tag_seed))
                    .alpns(vec![LUKE_TAG_ALPN.to_vec()])
                    .bind()
            );

            let social_ep = match social_res {
                Ok(ep) => ep,
                Err(e) => {
                    tracing::warn!("friends: falha ao iniciar endpoint social P2P: {e:#}");
                    return;
                }
            };
            *mgr.social_endpoint.lock() = Some(social_ep.clone());

            // Aguarda ambos conectarem ao Home Relay antes de anunciar para garantir RelayUrl imediato no Pkarr
            if let Ok(ref tag_ep) = tag_res {
                let _ = tokio::time::timeout(Duration::from_secs(4), async {
                    tokio::join!(social_ep.online(), tag_ep.online());
                })
                .await;
            } else {
                let _ = tokio::time::timeout(Duration::from_secs(4), social_ep.online()).await;
            }

            // 2. Servidor de Descoberta + Entrega Direta em 1 Hop (`LUKE_TAG_ALPN`)
            if let Ok(tag_ep) = tag_res {
                *mgr.tag_endpoint.lock() = Some(tag_ep.clone());
                let mgr_tag = mgr.clone();
                let nick_tag = nick_fn.clone();
                tokio::spawn(async move {
                    while !mgr_tag.stop.load(Ordering::SeqCst) {
                        let Some(inc) = tag_ep.accept().await else { break };
                        let mgr_conn = mgr_tag.clone();
                        let my_nick = nick_tag();
                        tokio::spawn(async move {
                            let Ok(conn) = inc.await else { return };
                            let remote_pk_bytes = *conn.remote_id().as_bytes();
                            let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };

                            // Passo 1: Envia TagCard assinado (com social_addr já preenchido!)
                            let card = mgr_conn.build_signed_tag_card(&my_nick);
                            if let Ok(bytes) = serde_json::to_vec(&card) {
                                let _ = send.write_all(&(bytes.len() as u32).to_be_bytes()).await;
                                let _ = send.write_all(&bytes).await;
                                let _ = send.flush().await;
                            }

                            // Passo 2: Aceita opcionalmente o FriendEnvelope já na mesma conexão QUIC!
                            if let Ok(Ok(SocialWire::Envelope { env, sender_addr })) =
                                tokio::time::timeout(Duration::from_secs(3), read_wire(&mut recv)).await
                            {
                                let remote_b58 = bs58::encode(remote_pk_bytes).into_string();
                                let mut ok = false;
                                let mut auto_accept = None;
                                if remote_b58 == env.from_pubkey_b58 && mgr_conn.verify_and_apply_envelope(&env).is_ok() {
                                    mgr_conn.update_friend_addr(&env.from_tag, sender_addr);
                                    ok = true;
                                    if env.action == "friend_request" {
                                        auto_accept = mgr_conn.maybe_build_auto_accept(&env.from_tag, &my_nick);
                                    }
                                }
                                let ack = SocialWire::EnvelopeAck {
                                    ok,
                                    from_tag: mgr_conn.tag.clone(),
                                    from_nick: my_nick.clone(),
                                    auto_accept,
                                    receiver_addr: mgr_conn.social_endpoint.lock().as_ref().map(|e| e.addr()),
                                };
                                let _ = write_wire(&mut send, &ack).await;
                            }

                            let _ = send.finish();
                            let _ = tokio::time::timeout(Duration::from_secs(3), send.stopped()).await;
                            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                        });
                    }
                });
            }

            // Se houver Cloudflare Worker configurado, publica TagCard assinado uma vez
            if let Some(worker_url) = configured_worker_url() {
                let card = mgr.build_signed_tag_card(&nick_fn());
                tokio::spawn(async move {
                    if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(4)).build() {
                        let _ = client
                            .post(format!("{worker_url}/v1/tag/register"))
                            .json(&card)
                            .send()
                            .await;
                    }
                });
            }

            tracing::info!("friends: Identidade {} online na rede P2P Iroh!", mgr.tag);

            // 3. Loop rápido de sincronização periódica (5s) + disparo inicial em 1.5s
            let mgr_hb = mgr.clone();
            let nick_hb = nick_fn.clone();
            let ticket_hb = ticket_fn.clone();
            let ep_hb = social_ep.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                while !mgr_hb.stop.load(Ordering::SeqCst) {
                    interval.tick().await;
                    let my_nick = nick_hb();
                    let am_streaming = ticket_hb().is_some();
                    mgr_hb.run_presence_and_retry_cycle(&ep_hb, &my_nick, am_streaming).await;
                }
            });

            // 4. Loop principal aceitando conexões autenticadas em LUKE_SOCIAL_ALPN
            while !mgr.stop.load(Ordering::SeqCst) {
                let Some(incoming) = social_ep.accept().await else { break };
                let mgr_conn = mgr.clone();
                let nick_conn = nick_fn.clone();
                let ticket_conn = ticket_fn.clone();

                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    let remote_pk_bytes = *conn.remote_id().as_bytes();
                    let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
                    let Ok(wire) = read_wire(&mut recv).await else { return };

                    mgr_conn
                        .handle_incoming_social_stream(
                            remote_pk_bytes,
                            wire,
                            &mut send,
                            &nick_conn(),
                            ticket_conn(),
                        )
                        .await;
                    let _ = send.finish();
                    let _ = tokio::time::timeout(Duration::from_secs(3), send.stopped()).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                });
            }
        });
    }

    async fn handle_incoming_social_stream<W: AsyncWriteExt + Unpin>(
        &self,
        remote_pk_bytes: [u8; 32],
        wire: SocialWire,
        send: &mut W,
        my_nick: &str,
        my_active_ticket: Option<String>,
    ) {
        match wire {
            SocialWire::Envelope { env, sender_addr } => {
                // Garante que quem abriu o TLS 1.3 QUIC é de fato dono da chave do envelope!
                let remote_b58 = bs58::encode(remote_pk_bytes).into_string();
                let mut ok = false;
                let mut auto_accept = None;
                if remote_b58 == env.from_pubkey_b58 && self.verify_and_apply_envelope(&env).is_ok() {
                    self.update_friend_addr(&env.from_tag, sender_addr);
                    ok = true;
                    if env.action == "friend_request" {
                        auto_accept = self.maybe_build_auto_accept(&env.from_tag, my_nick);
                    }
                }
                let ack = SocialWire::EnvelopeAck {
                    ok,
                    from_tag: self.tag.clone(),
                    from_nick: my_nick.to_string(),
                    auto_accept,
                    receiver_addr: self.social_endpoint.lock().as_ref().map(|e| e.addr()),
                };
                let _ = write_wire(send, &ack).await;
            }
            SocialWire::PresencePing {
                from_tag,
                from_nick,
                is_streaming,
                sender_addr,
            } => {
                let remote_b58 = bs58::encode(remote_pk_bytes).into_string();
                let is_accepted_friend = {
                    let mut lock = self.friends.write();
                    if let Some(f) = lock.get_mut(&from_tag) {
                        if f.status == FriendStatus::Accepted && f.public_key_b58 == remote_b58 {
                            f.nick = from_nick;
                            f.is_online = true;
                            f.is_streaming = is_streaming;
                            if sender_addr.is_some() {
                                f.last_addr = sender_addr;
                            }
                            f.last_seen = Some(Instant::now());
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if is_accepted_friend {
                    let pong = SocialWire::PresencePong {
                        from_tag: self.tag.clone(),
                        from_nick: my_nick.to_string(),
                        is_streaming: my_active_ticket.is_some(),
                        responder_addr: self.social_endpoint.lock().as_ref().map(|e| e.addr()),
                    };
                    let _ = write_wire(send, &pong).await;
                }
            }
            SocialWire::KnockRequest {
                request_id,
                from_tag,
                from_nick,
            } => {
                let remote_b58 = bs58::encode(remote_pk_bytes).into_string();
                let is_accepted_friend = {
                    let lock = self.friends.read();
                    lock.get(&from_tag)
                        .map(|f| f.status == FriendStatus::Accepted && f.public_key_b58 == remote_b58)
                        .unwrap_or(false)
                };

                if !is_accepted_friend {
                    let _ = write_wire(
                        send,
                        &SocialWire::KnockDecision {
                            request_id,
                            accepted: false,
                            room_ticket: None,
                            host_nick: my_nick.to_string(),
                            reason: Some("Apenas amigos confirmados mutuamente podem pedir para entrar.".into()),
                        },
                    )
                    .await;
                    return;
                }

                if my_active_ticket.is_none() {
                    let _ = write_wire(
                        send,
                        &SocialWire::KnockDecision {
                            request_id,
                            accepted: false,
                            room_ticket: None,
                            host_nick: my_nick.to_string(),
                            reason: Some("A transmissão já foi encerrada.".into()),
                        },
                    )
                    .await;
                    return;
                }

                let (tx, rx) = oneshot::channel::<bool>();
                self.pending_knocks.lock().insert(request_id.clone(), tx);
                self.notifications.lock().push_back(SocialNotification::IncomingKnock {
                    request_id: request_id.clone(),
                    from_tag: from_tag.clone(),
                    from_nick: from_nick.clone(),
                });

                let accepted = tokio::time::timeout(Duration::from_secs(30), rx)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or(false);

                self.pending_knocks.lock().remove(&request_id);

                let decision = if accepted {
                    SocialWire::KnockDecision {
                        request_id,
                        accepted: true,
                        room_ticket: my_active_ticket,
                        host_nick: my_nick.to_string(),
                        reason: None,
                    }
                } else {
                    SocialWire::KnockDecision {
                        request_id,
                        accepted: false,
                        room_ticket: None,
                        host_nick: my_nick.to_string(),
                        reason: Some("Pedido recusado ou expirado.".into()),
                    }
                };
                let _ = write_wire(send, &decision).await;
            }
            _ => {}
        }
    }

    pub fn respond_to_knock(&self, request_id: &str, accept: bool) -> bool {
        if let Some(tx) = self.pending_knocks.lock().remove(request_id) {
            let _ = tx.send(accept);
            true
        } else {
            false
        }
    }

    async fn resolve_and_deliver_via_tag(
        &self,
        target_tag: &str,
        env_opt: Option<&FriendEnvelope>,
    ) -> Result<(TagCard, bool)> {
        let seed = derive_tag_discovery_seed(target_tag);
        let target_id = SecretKey::from_bytes(&seed).public();

        let existing_ep = self.social_endpoint.lock().clone();
        let temp_ep = if existing_ep.is_none() {
            let ep = Endpoint::builder(presets::N0)
                .secret_key(SecretKey::from_bytes(&self.signing_key.to_bytes()))
                .bind()
                .await?;
            let _ = tokio::time::timeout(Duration::from_millis(2500), ep.online()).await;
            Some(ep)
        } else {
            None
        };
        let ep = existing_ep.as_ref().or(temp_ep.as_ref()).unwrap();
        let my_sender_addr = Some(ep.addr());

        let res = async {
            let conn = tokio::time::timeout(
                Duration::from_secs(10),
                ep.connect(EndpointAddr::from(target_id), LUKE_TAG_ALPN),
            )
            .await
            .map_err(|_| {
                anyhow!("Amigo offline no momento (pedido ficará pendente e será entregue automaticamente)")
            })??;

            let (mut send, mut recv) = conn.open_bi().await?;

            // CRÍTICO (RFC 9000 QUIC): escreve no `send` + `finish()` ANTES de ler do `recv`,
            // pois `open_bi()` só materializa o stream no `accept_bi()` remoto ao transmitir o primeiro frame STREAM!
            if let Some(env) = env_opt {
                write_wire(
                    &mut send,
                    &SocialWire::Envelope {
                        env: env.clone(),
                        sender_addr: my_sender_addr,
                    },
                )
                .await?;
            } else {
                let _ = send.write_all(&0u32.to_be_bytes()).await;
            }
            let _ = send.finish();

            let mut len_buf = [0u8; 4];
            tokio::time::timeout(Duration::from_secs(6), recv.read_exact(&mut len_buf))
                .await
                .map_err(|_| anyhow!("Tempo esgotado lendo TagCard"))??;
            let len = u32::from_be_bytes(len_buf) as usize;
            if len == 0 || len > 16 * 1024 {
                anyhow::bail!("TagCard inválido");
            }
            let mut buf = vec![0u8; len];
            tokio::time::timeout(Duration::from_secs(6), recv.read_exact(&mut buf))
                .await
                .map_err(|_| anyhow!("Tempo esgotado lendo payload do TagCard"))??;

            let card: TagCard = serde_json::from_slice(&buf)?;
            Self::verify_tag_card(&card)?;

            let mut delivered = false;
            if env_opt.is_some() {
                if let Ok(Ok(SocialWire::EnvelopeAck {
                    ok,
                    auto_accept,
                    receiver_addr,
                    ..
                })) = tokio::time::timeout(Duration::from_secs(4), read_wire(&mut recv)).await
                {
                    if let Some(addr) = receiver_addr {
                        self.update_friend_addr(&card.tag, Some(addr));
                    }
                    if let Some(acc_env) = auto_accept {
                        let _ = self.verify_and_apply_envelope(&acc_env);
                    }
                    delivered = ok;
                }
            }

            conn.close(0u32.into(), b"ok");
            Ok::<(TagCard, bool), anyhow::Error>((card, delivered))
        }
        .await;

        if let Some(t) = temp_ep {
            let _ = t.close().await;
        }

        if let Ok(pair) = res {
            return Ok(pair);
        }

        // Fallback via Cloudflare Worker se configurado
        if let Some(worker_url) = configured_worker_url() {
            if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(4)).build() {
                let encoded = target_tag.replace('#', "%23");
                if let Ok(resp) = client.get(format!("{worker_url}/v1/tag/{encoded}")).send().await {
                    if resp.status().is_success() {
                        if let Ok(card) = resp.json::<TagCard>().await {
                            if Self::verify_tag_card(&card).is_ok() {
                                return Ok((card, false));
                            }
                        }
                    }
                }
            }
        }

        res
    }

    async fn send_envelope_p2p(
        &self,
        pubkey_b58: &str,
        env: &FriendEnvelope,
        addr_override: Option<EndpointAddr>,
    ) -> Result<()> {
        // Envia também ao Cloudflare Worker se configurado (caixa postal offline)
        if let Some(worker_url) = configured_worker_url() {
            let env_cf = env.clone();
            tokio::spawn(async move {
                if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(4)).build() {
                    let _ = client.post(format!("{worker_url}/v1/inbox")).json(&env_cf).send().await;
                }
            });
        }

        let pk_bytes = bs58::decode(pubkey_b58).into_vec()?;
        let pk_arr: [u8; 32] = pk_bytes
            .try_into()
            .map_err(|_| anyhow!("Chave pública do amigo inválida"))?;
        let remote_id = PublicKey::from_bytes(&pk_arr)?;

        let existing_ep = self.social_endpoint.lock().clone();
        let temp_ep = if existing_ep.is_none() {
            let ep = Endpoint::builder(presets::N0)
                .secret_key(SecretKey::from_bytes(&self.signing_key.to_bytes()))
                .bind()
                .await?;
            let _ = tokio::time::timeout(Duration::from_millis(2500), ep.online()).await;
            Some(ep)
        } else {
            None
        };
        let ep = existing_ep.as_ref().or(temp_ep.as_ref()).unwrap();
        let my_sender_addr = Some(ep.addr());

        let res = async {
            // Tenta rota conhecida primeiro (`social_addr` / `last_addr`), com fallback automático para Pkarr
            let conn = if let Some(hint) = addr_override {
                match tokio::time::timeout(Duration::from_secs(6), ep.connect(hint, LUKE_SOCIAL_ALPN)).await {
                    Ok(Ok(c)) => c,
                    _ => tokio::time::timeout(
                        Duration::from_secs(8),
                        ep.connect(EndpointAddr::from(remote_id), LUKE_SOCIAL_ALPN),
                    )
                    .await
                    .map_err(|_| anyhow!("Tempo esgotado conectando ao amigo via P2P"))??,
                }
            } else {
                tokio::time::timeout(
                    Duration::from_secs(9),
                    ep.connect(EndpointAddr::from(remote_id), LUKE_SOCIAL_ALPN),
                )
                .await
                .map_err(|_| anyhow!("Tempo esgotado conectando ao amigo via P2P"))??
            };

            let (mut send, mut recv) = conn.open_bi().await?;
            write_wire(
                &mut send,
                &SocialWire::Envelope {
                    env: env.clone(),
                    sender_addr: my_sender_addr,
                },
            )
            .await?;
            let _ = send.finish();

            // CRÍTICO: Aguarda confirmação de leitura (`EnvelopeAck`) antes de fechar a conexão QUIC!
            // Caso contrário, o Drop de `Connection` envia CONNECTION_CLOSE imediato e aborta o stream em trânsito.
            match tokio::time::timeout(Duration::from_secs(6), read_wire(&mut recv)).await {
                Ok(Ok(SocialWire::EnvelopeAck {
                    ok,
                    auto_accept,
                    receiver_addr,
                    from_tag,
                    ..
                })) => {
                    if let Some(addr) = receiver_addr {
                        self.update_friend_addr(&from_tag, Some(addr));
                    }
                    if let Some(acc_env) = auto_accept {
                        let _ = self.verify_and_apply_envelope(&acc_env);
                    }
                    conn.close(0u32.into(), b"ack");
                    if !ok {
                        anyhow::bail!("Envelope rejeitado pelo destinatário");
                    }
                }
                _ => {
                    // Compatibilidade com nós v0.5.0: aguarda o par drenar/fechar o stream antes de dropar `conn`
                    let _ = tokio::time::timeout(Duration::from_millis(2500), send.stopped()).await;
                    conn.close(0u32.into(), b"done");
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Some(t) = temp_ep {
            let _ = t.close().await;
        }
        res
    }

    pub async fn send_friend_request(&self, raw_tag: &str, my_nick: &str) -> Result<String> {
        let target_tag = normalize_tag(raw_tag)?;
        if target_tag == self.tag {
            anyhow::bail!("Você não pode adicionar sua própria tag ({target_tag}).");
        }

        // Se o amigo já nos enviou pedido (PendingIn), promove imediatamente a Accepted!
        let existing_in = {
            let lock = self.friends.read();
            lock.get(&target_tag).cloned()
        };
        if let Some(f) = existing_in {
            if f.status == FriendStatus::PendingIn {
                self.respond_friend_request(&target_tag, true, my_nick).await?;
                return Ok(format!("Amizade confirmada com {} ({target_tag})!", f.nick));
            }
            if f.status == FriendStatus::Accepted {
                return Ok(format!("{} ({target_tag}) já está na sua lista de amigos.", f.nick));
            }
        }

        // Registra como PendingOut imediatamente
        {
            let mut lock = self.friends.write();
            lock.entry(target_tag.clone()).or_insert(Friend {
                tag: target_tag.clone(),
                nick: target_tag.clone(),
                public_key_b58: String::new(),
                status: FriendStatus::PendingOut,
                is_online: false,
                is_streaming: false,
                last_addr: None,
                last_seen: None,
            });
        }
        self.save_friends();

        let env = self.sign_envelope(&target_tag, my_nick, "friend_request");

        match self.resolve_and_deliver_via_tag(&target_tag, Some(&env)).await {
            Ok((card, delivered_via_tag)) => {
                {
                    let mut lock = self.friends.write();
                    if let Some(f) = lock.get_mut(&target_tag) {
                        f.nick = card.nick.clone();
                        f.public_key_b58 = card.pubkey_b58.clone();
                        if card.social_addr.is_some() {
                            f.last_addr = card.social_addr.clone();
                        }
                    }
                }
                self.save_friends();

                if !delivered_via_tag {
                    let _ = self
                        .send_envelope_p2p(&card.pubkey_b58, &env, card.social_addr.clone())
                        .await;
                }

                let now_accepted = self
                    .friends
                    .read()
                    .get(&target_tag)
                    .map(|f| f.status == FriendStatus::Accepted)
                    .unwrap_or(false);

                if now_accepted {
                    Ok(format!(
                        "Amizade mútua confirmada com {} ({target_tag})!",
                        card.nick
                    ))
                } else {
                    Ok(format!(
                        "Pedido entregue para {} ({target_tag})! Aguardando confirmação.",
                        card.nick
                    ))
                }
            }
            Err(_) => {
                // Se Worker estiver ativo, tenta deixar na inbox mesmo sem P2P imediato
                if let Some(worker_url) = configured_worker_url() {
                    if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(4)).build() {
                        let _ = client.post(format!("{worker_url}/v1/inbox")).json(&env).send().await;
                    }
                }
                Ok(format!(
                    "Pedido salvo para {target_tag}! Será entregue automaticamente em segundos assim que ele estiver online."
                ))
            }
        }
    }

    pub async fn respond_friend_request(&self, friend_tag: &str, accept: bool, my_nick: &str) -> Result<()> {
        let norm = normalize_tag(friend_tag)?;
        let (pubkey_opt, addr_opt) = {
            let mut lock = self.friends.write();
            if accept {
                if let Some(f) = lock.get_mut(&norm) {
                    f.status = FriendStatus::Accepted;
                    (Some(f.public_key_b58.clone()), f.last_addr.clone())
                } else {
                    (None, None)
                }
            } else {
                match lock.remove(&norm) {
                    Some(f) => (Some(f.public_key_b58), f.last_addr),
                    None => (None, None),
                }
            }
        };
        self.save_friends();

        if let Some(pk) = pubkey_opt {
            if !pk.is_empty() {
                let action = if accept { "friend_accept" } else { "friend_reject" };
                let env = self.sign_envelope(&norm, my_nick, action);
                let _ = self.send_envelope_p2p(&pk, &env, addr_opt).await;
            }
        }
        Ok(())
    }

    pub fn remove_friend(&self, friend_tag: &str) -> Result<()> {
        let norm = normalize_tag(friend_tag)?;
        self.friends.write().remove(&norm);
        self.save_friends();
        Ok(())
    }

    /// Pede para entrar na transmissão de um amigo confirmado (`Accepted`) via QUIC P2P direto.
    pub async fn knock_friend_stream(
        &self,
        friend_tag: &str,
        my_nick: &str,
        addr_override: Option<EndpointAddr>,
    ) -> Result<KnockResult> {
        let norm = normalize_tag(friend_tag)?;
        let friend = {
            let lock = self.friends.read();
            lock.get(&norm).cloned()
        }
        .ok_or_else(|| anyhow!("Amigo {norm} não encontrado."))?;

        if friend.status != FriendStatus::Accepted {
            anyhow::bail!("A amizade com {norm} ainda não foi confirmada por ambos.");
        }
        if friend.public_key_b58.is_empty() {
            anyhow::bail!("Chave pública do amigo indisponível.");
        }

        let pk_bytes = bs58::decode(&friend.public_key_b58).into_vec()?;
        let pk_arr: [u8; 32] = pk_bytes
            .try_into()
            .map_err(|_| anyhow!("Chave pública corrompida"))?;
        let remote_id = PublicKey::from_bytes(&pk_arr)?;

        let ep_opt = self.social_endpoint.lock().clone();
        let temp_ep;
        let ep = if let Some(ref existing) = ep_opt {
            existing
        } else {
            temp_ep = Endpoint::builder(presets::N0)
                .secret_key(SecretKey::from_bytes(&self.signing_key.to_bytes()))
                .bind()
                .await?;
            let _ = tokio::time::timeout(Duration::from_millis(2500), temp_ep.online()).await;
            &temp_ep
        };

        let hint = addr_override.or(friend.last_addr);
        let conn = if let Some(h) = hint {
            match tokio::time::timeout(Duration::from_secs(6), ep.connect(h, LUKE_SOCIAL_ALPN)).await {
                Ok(Ok(c)) => c,
                _ => tokio::time::timeout(
                    Duration::from_secs(8),
                    ep.connect(EndpointAddr::from(remote_id), LUKE_SOCIAL_ALPN),
                )
                .await
                .map_err(|_| anyhow!("Não foi possível alcançar {}. Verifique se ele está online.", friend.nick))??,
            }
        } else {
            tokio::time::timeout(
                Duration::from_secs(8),
                ep.connect(EndpointAddr::from(remote_id), LUKE_SOCIAL_ALPN),
            )
            .await
            .map_err(|_| anyhow!("Não foi possível alcançar {}. Verifique se ele está online.", friend.nick))??
        };

        let (mut send, mut recv) = conn.open_bi().await?;
        let req_id = Uuid::new_v4().to_string();
        write_wire(
            &mut send,
            &SocialWire::KnockRequest {
                request_id: req_id.clone(),
                from_tag: self.tag.clone(),
                from_nick: my_nick.to_string(),
            },
        )
        .await?;
        let _ = send.finish();

        let resp = tokio::time::timeout(Duration::from_secs(33), read_wire(&mut recv))
            .await
            .map_err(|_| anyhow!("Tempo esgotado aguardando resposta de {}.", friend.nick))??;
        conn.close(0u32.into(), b"ok");

        match resp {
            SocialWire::KnockDecision {
                accepted,
                room_ticket,
                host_nick,
                reason,
                ..
            } => {
                if accepted && room_ticket.is_some() {
                    Ok(KnockResult {
                        accepted: true,
                        room_ticket,
                        host_nick: host_nick.clone(),
                        message: format!("{host_nick} permitiu sua entrada! Conectando..."),
                    })
                } else {
                    Ok(KnockResult {
                        accepted: false,
                        room_ticket: None,
                        host_nick,
                        message: reason.unwrap_or_else(|| "Entrada não autorizada pelo anfitrião.".into()),
                    })
                }
            }
            _ => Err(anyhow!("Resposta inesperada ao pedido de entrada.")),
        }
    }

    async fn run_presence_and_retry_cycle(&self, ep: &Endpoint, my_nick: &str, am_streaming: bool) {
        // 1. Se houver Cloudflare Worker configurado, consome envelopes offline autenticados
        if let Some(worker_url) = configured_worker_url() {
            if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(3)).build() {
                let ts = now_unix_secs();
                let auth_msg = format!("INBOX:{}:{ts}", self.tag);
                let sig = hex::encode(self.signing_key.sign(auth_msg.as_bytes()).to_bytes());
                let encoded_tag = self.tag.replace('#', "%23");
                if let Ok(resp) = client
                    .get(format!("{worker_url}/v1/inbox/{encoded_tag}?ts={ts}&sig={sig}"))
                    .send()
                    .await
                {
                    if let Ok(envelopes) = resp.json::<Vec<FriendEnvelope>>().await {
                        for env in envelopes {
                            let _ = self.verify_and_apply_envelope(&env);
                        }
                    }
                }
            }
        }

        let snapshot: Vec<Friend> = self.friends.read().values().cloned().collect();
        for f in snapshot {
            match f.status {
                FriendStatus::PendingOut => {
                    let env = self.sign_envelope(&f.tag, my_nick, "friend_request");
                    if !f.public_key_b58.is_empty() {
                        if self
                            .send_envelope_p2p(&f.public_key_b58, &env, f.last_addr.clone())
                            .await
                            .is_err()
                        {
                            // Tenta também via endpoint de Tag caso o amigo tenha acabado de abrir o app
                            let _ = self.resolve_and_deliver_via_tag(&f.tag, Some(&env)).await;
                        }
                    } else if let Ok((card, delivered)) =
                        self.resolve_and_deliver_via_tag(&f.tag, Some(&env)).await
                    {
                        {
                            let mut lock = self.friends.write();
                            if let Some(entry) = lock.get_mut(&f.tag) {
                                entry.nick = card.nick.clone();
                                entry.public_key_b58 = card.pubkey_b58.clone();
                                if card.social_addr.is_some() {
                                    entry.last_addr = card.social_addr.clone();
                                }
                            }
                        }
                        self.save_friends();
                        if !delivered {
                            let _ = self
                                .send_envelope_p2p(&card.pubkey_b58, &env, card.social_addr)
                                .await;
                        }
                    }
                }
                FriendStatus::Accepted if !f.public_key_b58.is_empty() => {
                    let Ok(pk_bytes) = bs58::decode(&f.public_key_b58).into_vec() else { continue };
                    let Ok(pk_arr): Result<[u8; 32], _> = pk_bytes.try_into() else { continue };
                    let Ok(remote_id) = PublicKey::from_bytes(&pk_arr) else { continue };

                    let ep_clone = ep.clone();
                    let my_tag = self.tag.clone();
                    let nick = my_nick.to_string();
                    let friends_arc = self.friends.clone();
                    let f_tag = f.tag.clone();
                    let f_hint = f.last_addr.clone();

                    tokio::spawn(async move {
                        let ping_res = async {
                            let my_addr = Some(ep_clone.addr());
                            let conn = if let Some(hint) = f_hint {
                                match tokio::time::timeout(
                                    Duration::from_millis(3500),
                                    ep_clone.connect(hint, LUKE_SOCIAL_ALPN),
                                )
                                .await
                                {
                                    Ok(Ok(c)) => c,
                                    _ => tokio::time::timeout(
                                        Duration::from_millis(4500),
                                        ep_clone.connect(EndpointAddr::from(remote_id), LUKE_SOCIAL_ALPN),
                                    )
                                    .await??,
                                }
                            } else {
                                tokio::time::timeout(
                                    Duration::from_millis(4500),
                                    ep_clone.connect(EndpointAddr::from(remote_id), LUKE_SOCIAL_ALPN),
                                )
                                .await??
                            };

                            let (mut send, mut recv) = conn.open_bi().await?;
                            write_wire(
                                &mut send,
                                &SocialWire::PresencePing {
                                    from_tag: my_tag,
                                    from_nick: nick,
                                    is_streaming: am_streaming,
                                    sender_addr: my_addr,
                                },
                            )
                            .await?;
                            let _ = send.finish();
                            let pong = tokio::time::timeout(Duration::from_millis(3500), read_wire(&mut recv)).await??;
                            conn.close(0u32.into(), b"ok");
                            Ok::<SocialWire, anyhow::Error>(pong)
                        }
                        .await;

                        if let Ok(SocialWire::PresencePong {
                            from_nick,
                            is_streaming,
                            responder_addr,
                            ..
                        }) = ping_res
                        {
                            let mut lock = friends_arc.write();
                            if let Some(entry) = lock.get_mut(&f_tag) {
                                entry.nick = from_nick;
                                entry.is_online = true;
                                entry.is_streaming = is_streaming;
                                if responder_addr.is_some() {
                                    entry.last_addr = responder_addr;
                                }
                                entry.last_seen = Some(Instant::now());
                            }
                        }
                    });
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_tag_cannot_be_forged() {
        let alice = FriendsManager::ephemeral_for_test();
        let bob = FriendsManager::ephemeral_for_test();
        let attacker = FriendsManager::ephemeral_for_test();

        assert!(alice.tag.starts_with("LK#"));
        assert_eq!(alice.tag.len(), 7);

        // Pedido legítimo de Alice para Bob passa na verificação Ed25519
        let valid_env = alice.sign_envelope(&bob.tag, "Alice", "friend_request");
        let res = bob.verify_and_apply_envelope(&valid_env).unwrap();
        assert!(res.is_some());
        assert_eq!(bob.list_friends()[0].status, FriendStatus::PendingIn);

        // Atacante tenta falsificar a tag da Alice usando sua própria chave privada
        let mut forged = attacker.sign_envelope(&bob.tag, "FakeAlice", "friend_accept");
        forged.from_tag = alice.tag.clone();
        assert!(
            bob.verify_and_apply_envelope(&forged).is_err(),
            "Envelope com LK#TAG falsificada deve ser rejeitado matematicamente!"
        );
    }

    #[tokio::test]
    async fn mutual_friendship_and_knock_p2p_e2e() -> Result<()> {
        let alice = FriendsManager::ephemeral_for_test();
        let bob = FriendsManager::ephemeral_for_test();

        // 1. Alice envia pedido -> Bob recebe (PendingIn); Bob aceita -> Alice confirma (Accepted)
        let req = alice.sign_envelope(&bob.tag, "Alice", "friend_request");
        {
            alice.friends.write().insert(
                bob.tag.clone(),
                Friend {
                    tag: bob.tag.clone(),
                    nick: "Bob".into(),
                    public_key_b58: bob.public_key_b58(),
                    status: FriendStatus::PendingOut,
                    is_online: true,
                    is_streaming: false,
                    last_addr: None,
                    last_seen: Some(Instant::now()),
                },
            );
        }
        bob.verify_and_apply_envelope(&req)?;
        assert_eq!(bob.friends.read()[&alice.tag].status, FriendStatus::PendingIn);

        // Bob aceita
        bob.friends.write().get_mut(&alice.tag).unwrap().status = FriendStatus::Accepted;
        let acc = bob.sign_envelope(&alice.tag, "Bob", "friend_accept");
        alice.verify_and_apply_envelope(&acc)?;
        assert_eq!(alice.friends.read()[&bob.tag].status, FriendStatus::Accepted);

        // 2. Bob está transmitindo ("lk-secret-stream-ticket") e escutando em LUKE_SOCIAL_ALPN
        let bob_ep = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&bob.signing_key.to_bytes()))
            .alpns(vec![LUKE_SOCIAL_ALPN.to_vec()])
            .bind()
            .await?;
        let bob_addr = bob_ep.addr();
        *bob.social_endpoint.lock() = Some(bob_ep.clone());

        let alice_ep = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&alice.signing_key.to_bytes()))
            .alpns(vec![LUKE_SOCIAL_ALPN.to_vec()])
            .bind()
            .await?;
        *alice.social_endpoint.lock() = Some(alice_ep.clone());

        let bob_srv = bob.clone();
        let srv_handle = tokio::spawn(async move {
            let inc = bob_ep.accept().await.unwrap();
            let conn = inc.await.unwrap();
            let remote_pk = *conn.remote_id().as_bytes();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let wire = read_wire(&mut recv).await.unwrap();
            bob_srv
                .handle_incoming_social_stream(
                    remote_pk,
                    wire,
                    &mut send,
                    "BobHost",
                    Some("lk-secret-stream-ticket".into()),
                )
                .await;
            let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
        });

        // Aprovação automática do toast no lado de Bob assim que o Knock chegar
        let bob_approver = bob.clone();
        tokio::spawn(async move {
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let notes = bob_approver.drain_notifications();
                for n in notes {
                    if let SocialNotification::IncomingKnock { request_id, .. } = n {
                        bob_approver.respond_to_knock(&request_id, true);
                        return;
                    }
                }
            }
        });

        let knock_res = alice.knock_friend_stream(&bob.tag, "Alice", Some(bob_addr)).await?;
        assert!(knock_res.accepted);
        assert_eq!(knock_res.room_ticket.as_deref(), Some("lk-secret-stream-ticket"));

        let _ = srv_handle.await;
        let _ = alice_ep.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn real_internet_pkarr_friend_request_and_accept_e2e() -> Result<()> {
        let alice = FriendsManager::ephemeral_for_test();
        let bob = FriendsManager::ephemeral_for_test();

        alice.start_daemons(Arc::new(|| "AliceReal".to_string()), Arc::new(|| None));
        bob.start_daemons(
            Arc::new(|| "BobReal".to_string()),
            Arc::new(|| Some("lk-live-room".to_string())),
        );

        // Aguarda ambos publicarem no Pkarr/Relay real do Iroh
        for _ in 0..40 {
            let a_ready = alice.social_endpoint.lock().is_some() && alice.tag_endpoint.lock().is_some();
            let b_ready = bob.social_endpoint.lock().is_some() && bob.tag_endpoint.lock().is_some();
            if a_ready && b_ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // 1. Alice adiciona Bob APENAS pela tag LK#XXXX (zero addr_override!)
        let msg = alice.send_friend_request(&bob.tag, "AliceReal").await?;
        println!("Alice send_friend_request -> {msg}");

        // Aguarda até 6s caso precise de 1 retry ciclo, mas na entrega em 1 hop já chega na hora!
        let mut bob_received = false;
        for _ in 0..30 {
            let list = bob.list_friends();
            if list
                .iter()
                .any(|f| f.tag == alice.tag && f.status == FriendStatus::PendingIn)
            {
                bob_received = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            bob_received,
            "Bob DEVE receber o pedido PendingIn de Alice via rede P2P real!"
        );

        // 2. Bob clica em Aceitar -> Alice DEVE transicionar para Accepted via P2P real!
        bob.respond_friend_request(&alice.tag, true, "BobReal").await?;

        let mut alice_accepted = false;
        for _ in 0..30 {
            let list = alice.list_friends();
            if list
                .iter()
                .any(|f| f.tag == bob.tag && f.status == FriendStatus::Accepted)
            {
                alice_accepted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            alice_accepted,
            "Alice DEVE receber a confirmação Accepted de Bob via rede P2P real!"
        );

        alice.stop.store(true, Ordering::SeqCst);
        bob.stop.store(true, Ordering::SeqCst);
        Ok(())
    }
}
