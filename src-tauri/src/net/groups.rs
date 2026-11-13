//! Grupos Persistentes P2P (estilo Discord, sem chamada de voz).
//!
//! Arquitetura:
//! - Cada Grupo possui identidade própria (`id`), salas de stream (`# Geral`, `# Jogos`, etc.)
//!   e lista de membros identificados por sua Tag permanente (`LK#XXXX`) + chave pública Ed25519.
//! - A sincronização de estado (salas, novos membros, quem está em qual sala e tickets ativos de
//!   Tela/Webcam) trafega diretamente em malha QUIC autenticada sobre `LUKE_GROUP_ALPN` no mesmo
//!   `Endpoint` social Ed25519 do usuário (`FriendsManager`), sem servidor central de mídia.
//! - Convites suportam tanto código curto temporário (`LKG-XXXX-XXXX` via Rendezvous P2P +
//!   Cloudflare Worker zero-knowledge `/v1/group/`) quanto ticket direto persistente (`lkg-...`).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use iroh::{endpoint::presets, Endpoint, EndpointAddr, EndpointId, SecretKey};
use parking_lot::{Mutex, RwLock};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::net::friends::{derive_tag, get_data_dir};
use crate::net::short_code::{configured_worker_url, hkdf_expand_32, hkdf_extract, SAFE_ALPHABET};

pub const LUKE_GROUP_ALPN: &[u8] = b"luke-share-group/1.0";
const LUKE_GROUP_INVITE_RV_ALPN: &[u8] = b"luke-share-group-rv/1.0";
const LUKE_GROUP_INVITE_IKM: &[u8] = b"LUKE-SHARE-V10-GROUP-INVITE-HKDF-KEY-2026";
const LUKE_GROUP_INVITE_AAD: &[u8] = b"luke-share-v1.0-group-aead";

const PRESENCE_TTL_SECS: u64 = 24;

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GroupRoom {
    pub id: String,
    pub name: String,
    pub created_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupMember {
    pub tag: String,
    pub nick: String,
    pub pubkey_b58: String,
    pub last_addr: Option<EndpointAddr>,
    pub joined_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomPresence {
    pub tag: String,
    pub nick: String,
    pub room_id: Option<String>,
    pub screen_ticket: Option<String>,
    pub camera_ticket: Option<String>,
    pub updated_at_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupRecord {
    pub id: String,
    pub name: String,
    pub owner_tag: String,
    pub secret_hex: String,
    pub rooms: Vec<GroupRoom>,
    pub members: HashMap<String, GroupMember>,
    pub created_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupSnapshot {
    pub id: String,
    pub name: String,
    pub owner_tag: String,
    pub secret_hex: String,
    pub rooms: Vec<GroupRoom>,
    pub members: Vec<GroupMember>,
    pub created_at: u64,
}

impl From<&GroupRecord> for GroupSnapshot {
    fn from(g: &GroupRecord) -> Self {
        let mut members: Vec<GroupMember> = g.members.values().cloned().collect();
        members.sort_by_key(|m| m.joined_at);
        Self {
            id: g.id.clone(),
            name: g.name.clone(),
            owner_tag: g.owner_tag.clone(),
            secret_hex: g.secret_hex.clone(),
            rooms: g.rooms.clone(),
            members,
            created_at: g.created_at,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PendingGroupInvite {
    pub group_id: String,
    pub group_name: String,
    pub from_tag: String,
    pub from_nick: String,
    pub snapshot: GroupSnapshot,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PendingGroupInviteSummary {
    pub group_id: String,
    pub group_name: String,
    pub from_tag: String,
    pub from_nick: String,
    pub rooms_count: usize,
    pub members_count: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ParticipantView {
    pub tag: String,
    pub nick: String,
    pub is_self: bool,
    pub screen_ticket: Option<String>,
    pub camera_ticket: Option<String>,
    pub has_screen: bool,
    pub has_camera: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupRoomView {
    pub id: String,
    pub name: String,
    pub participants: Vec<ParticipantView>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub owner_tag: String,
    pub is_owner: bool,
    pub rooms: Vec<GroupRoomView>,
    pub members_count: usize,
    pub online_members_count: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupsStateSnapshot {
    pub groups: Vec<GroupView>,
    pub active_group_id: Option<String>,
    pub active_room_id: Option<String>,
    pub pending_invites: Vec<PendingGroupInviteSummary>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GroupInviteBundle {
    pub short_code: Option<String>,
    pub direct_ticket: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupWire {
    Sync {
        group_id: String,
        group_name: String,
        owner_tag: String,
        secret_hex: String,
        rooms: Vec<GroupRoom>,
        members: Vec<GroupMember>,
        from_member: GroupMember,
        presence: Option<RoomPresence>,
        sender_addr: Option<EndpointAddr>,
    },
    SyncAck {
        ok: bool,
        rooms: Vec<GroupRoom>,
        members: Vec<GroupMember>,
        presences: Vec<RoomPresence>,
        responder_addr: Option<EndpointAddr>,
    },
    InvitePush {
        from_tag: String,
        from_nick: String,
        from_pubkey_b58: String,
        snapshot: GroupSnapshot,
        sender_addr: Option<EndpointAddr>,
    },
    InviteAck {
        ok: bool,
    },
    RedeemInvite {
        code: String,
        joiner: GroupMember,
        sender_addr: Option<EndpointAddr>,
    },
    RedeemAck {
        ok: bool,
        snapshot: Option<GroupSnapshot>,
        reason: Option<String>,
    },
}

#[derive(Clone)]
struct LivePresenceEntry {
    presence: RoomPresence,
    received_at: Instant,
}

struct ActiveGroupInvite {
    group_id: String,
    #[allow(dead_code)]
    created_at: Instant,
    rv_stop: Arc<AtomicBool>,
}

impl Drop for ActiveGroupInvite {
    fn drop(&mut self) {
        self.rv_stop.store(true, Ordering::SeqCst);
    }
}

pub type GetActiveTrackTicketsFn = Arc<dyn Fn() -> (Option<String>, Option<String>) + Send + Sync>;

pub struct GroupManager {
    signing_key: SigningKey,
    #[allow(dead_code)]
    verifying_key: VerifyingKey,
    pub my_tag: String,
    pub my_pubkey_b58: String,
    storage_path: Option<PathBuf>,
    groups: Arc<RwLock<HashMap<String, GroupRecord>>>,
    /// `group_id -> (tag -> LivePresenceEntry)`
    presences: Arc<RwLock<HashMap<String, HashMap<String, LivePresenceEntry>>>>,
    pending_invites: Arc<Mutex<Vec<PendingGroupInvite>>>,
    toasts: Arc<Mutex<VecDeque<(String, String)>>>,
    active_invites: Arc<Mutex<HashMap<String, ActiveGroupInvite>>>,
    active_group_id: Arc<RwLock<Option<String>>>,
    active_room_id: Arc<RwLock<Option<String>>>,
    endpoint: Arc<Mutex<Option<Endpoint>>>,
    get_nick: Arc<RwLock<Option<Arc<dyn Fn() -> String + Send + Sync>>>>,
    get_tracks: Arc<RwLock<Option<GetActiveTrackTicketsFn>>>,
    stop: Arc<AtomicBool>,
}

async fn write_group_wire<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &GroupWire) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

async fn read_group_wire<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<GroupWire> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > 512 * 1024 {
        anyhow::bail!("Mensagem de grupo com tamanho inválido ({len})");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

fn decode_pubkey_to_endpoint_id(pubkey_b58: &str) -> Result<EndpointId> {
    let bytes = bs58::decode(pubkey_b58)
        .into_vec()
        .context("pubkey Base58 inválida")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("Chave Ed25519 deve ter 32 bytes"))?;
    let vk = VerifyingKey::from_bytes(&arr).context("Chave Ed25519 malformada")?;
    EndpointId::from_bytes(vk.as_bytes()).map_err(|e| anyhow!("EndpointId inválido: {e}"))
}

pub fn generate_group_short_code() -> String {
    let mut rng = rand::rng();
    let mut rand_bytes = [0u8; 8];
    rng.fill(&mut rand_bytes);

    let mut chars = [0u8; 8];
    for i in 0..8 {
        chars[i] = SAFE_ALPHABET[(rand_bytes[i] as usize) & 31];
    }
    let part1 = std::str::from_utf8(&chars[0..4]).unwrap_or("ABCD");
    let part2 = std::str::from_utf8(&chars[4..8]).unwrap_or("EFGH");
    format!("LKG-{part1}-{part2}")
}

pub fn normalize_group_short_code(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.len() > 24 || trimmed.starts_with("lkg-") {
        return None;
    }
    let upper_alnum: String = trimmed
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();

    let core = if upper_alnum.len() == 11 && upper_alnum.starts_with("LKG") {
        &upper_alnum[3..]
    } else {
        return None;
    };

    if core.chars().all(|c| SAFE_ALPHABET.contains(&(c as u8))) {
        Some(core.to_string())
    } else {
        None
    }
}

struct DerivedGroupCodeKeys {
    aes_key: [u8; 32],
    kv_blind_hash: String,
    rv_secret_seed: [u8; 32],
}

fn derive_group_code_keys(code: &str) -> Result<DerivedGroupCodeKeys> {
    let core = normalize_group_short_code(code)
        .ok_or_else(|| anyhow!("Código de grupo inválido (esperado LKG-XXXX-XXXX)"))?;
    let prk = hkdf_extract(LUKE_GROUP_INVITE_IKM, core.as_bytes());
    let aes_key = hkdf_expand_32(&prk, b"luke-share/v1.0/group-aes-256-gcm");
    let kv_raw = hkdf_expand_32(&prk, b"luke-share/v1.0/group-kv-blind-hash");
    let rv_secret_seed = hkdf_expand_32(&prk, b"luke-share/v1.0/group-iroh-rv-seed");
    Ok(DerivedGroupCodeKeys {
        aes_key,
        kv_blind_hash: hex::encode(&kv_raw[..16]),
        rv_secret_seed,
    })
}

fn encrypt_group_payload_for_code(code: &str, payload: &[u8]) -> Result<Vec<u8>> {
    let keys = derive_group_code_keys(code)?;
    let cipher = Aes256Gcm::new_from_slice(&keys.aes_key)
        .map_err(|e| anyhow!("AES-256-GCM init error: {e}"))?;
    let compressed = miniz_oxide::deflate::compress_to_vec(payload, 6);
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);
    let ct = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &compressed,
                aad: LUKE_GROUP_INVITE_AAD,
            },
        )
        .map_err(|e| anyhow!("Falha cifrando convite de grupo: {e}"))?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn decrypt_group_payload_for_code(code: &str, blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 13 {
        anyhow::bail!("Payload de grupo corrompido");
    }
    let keys = derive_group_code_keys(code)?;
    let cipher = Aes256Gcm::new_from_slice(&keys.aes_key)
        .map_err(|e| anyhow!("AES-256-GCM init error: {e}"))?;
    let nonce_arr: [u8; 12] = blob[..12]
        .try_into()
        .map_err(|_| anyhow!("Nonce inválido no payload de grupo"))?;
    let nonce = Nonce::from(nonce_arr);
    let compressed = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &blob[12..],
                aad: LUKE_GROUP_INVITE_AAD,
            },
        )
        .map_err(|_| anyhow!("Falha ao autenticar convite de grupo (código incorreto)"))?;
    miniz_oxide::inflate::decompress_to_vec(&compressed)
        .map_err(|e| anyhow!("Falha ao descomprimir convite de grupo: {e:?}"))
}

pub fn encode_direct_group_ticket(snap: &GroupSnapshot) -> Result<String> {
    let json = serde_json::to_vec(snap)?;
    let compressed = miniz_oxide::deflate::compress_to_vec(&json, 6);
    let b64 = crate::pipeline::base64_encode(&compressed);
    Ok(format!("lkg-{b64}"))
}

pub fn decode_direct_group_ticket(raw: &str) -> Result<GroupSnapshot> {
    let trimmed = raw.trim();
    let payload_b64 = trimmed
        .strip_prefix("lkg-")
        .ok_or_else(|| anyhow!("Ticket de grupo deve começar com lkg-"))?;
    let compressed = base64_decode_loose(payload_b64)?;
    let json = miniz_oxide::inflate::decompress_to_vec(&compressed)
        .map_err(|e| anyhow!("Falha ao descomprimir ticket de grupo: {e:?}"))?;
    Ok(serde_json::from_slice(&json)?)
}

fn base64_decode_loose(input: &str) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => anyhow::bail!("caractere inválido no ticket de grupo: {}", b as char),
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            v.push((buf >> bits) as u8);
        }
    }
    Ok(v)
}

impl GroupManager {
    pub fn load_or_create(signing_key: SigningKey) -> Result<Arc<Self>> {
        let verifying_key = signing_key.verifying_key();
        let my_tag = derive_tag(&verifying_key);
        let my_pubkey_b58 = bs58::encode(verifying_key.as_bytes()).into_string();

        let dir = get_data_dir()?;
        let storage_path = dir.join("groups.json");

        let mut map = HashMap::new();
        if storage_path.exists() {
            if let Ok(content) = fs::read_to_string(&storage_path) {
                if let Ok(saved) = serde_json::from_str::<Vec<GroupRecord>>(&content) {
                    for g in saved {
                        map.insert(g.id.clone(), g);
                    }
                }
            }
        }

        Ok(Arc::new(Self {
            signing_key,
            verifying_key,
            my_tag,
            my_pubkey_b58,
            storage_path: Some(storage_path),
            groups: Arc::new(RwLock::new(map)),
            presences: Arc::new(RwLock::new(HashMap::new())),
            pending_invites: Arc::new(Mutex::new(Vec::new())),
            toasts: Arc::new(Mutex::new(VecDeque::new())),
            active_invites: Arc::new(Mutex::new(HashMap::new())),
            active_group_id: Arc::new(RwLock::new(None)),
            active_room_id: Arc::new(RwLock::new(None)),
            endpoint: Arc::new(Mutex::new(None)),
            get_nick: Arc::new(RwLock::new(None)),
            get_tracks: Arc::new(RwLock::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        }))
    }

    #[cfg(test)]
    pub fn ephemeral_for_test() -> Arc<Self> {
        let mut rng = rand::rng();
        let signing_key = SigningKey::generate(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let my_tag = derive_tag(&verifying_key);
        let my_pubkey_b58 = bs58::encode(verifying_key.as_bytes()).into_string();
        Arc::new(Self {
            signing_key,
            verifying_key,
            my_tag,
            my_pubkey_b58,
            storage_path: None,
            groups: Arc::new(RwLock::new(HashMap::new())),
            presences: Arc::new(RwLock::new(HashMap::new())),
            pending_invites: Arc::new(Mutex::new(Vec::new())),
            toasts: Arc::new(Mutex::new(VecDeque::new())),
            active_invites: Arc::new(Mutex::new(HashMap::new())),
            active_group_id: Arc::new(RwLock::new(None)),
            active_room_id: Arc::new(RwLock::new(None)),
            endpoint: Arc::new(Mutex::new(None)),
            get_nick: Arc::new(RwLock::new(None)),
            get_tracks: Arc::new(RwLock::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    fn save(&self) {
        let Some(ref path) = self.storage_path else {
            return;
        };
        let list: Vec<GroupRecord> = self.groups.read().values().cloned().collect();
        if let Ok(json) = serde_json::to_string_pretty(&list) {
            let _ = fs::write(path, json);
        }
    }

    fn current_nick(&self) -> String {
        self.get_nick
            .read()
            .as_ref()
            .map(|f| f())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.my_tag.clone())
    }

    fn current_tracks(&self) -> (Option<String>, Option<String>) {
        self.get_tracks
            .read()
            .as_ref()
            .map(|f| f())
            .unwrap_or((None, None))
    }

    fn my_member_record(&self, nick: &str) -> GroupMember {
        let addr = self.endpoint.lock().as_ref().map(|e| e.addr());
        GroupMember {
            tag: self.my_tag.clone(),
            nick: nick.to_string(),
            pubkey_b58: self.my_pubkey_b58.clone(),
            last_addr: addr,
            joined_at: now_unix_secs(),
        }
    }

    pub fn attach_and_start(
        self: &Arc<Self>,
        endpoint: Endpoint,
        get_nick: Arc<dyn Fn() -> String + Send + Sync>,
        get_tracks: GetActiveTrackTicketsFn,
    ) {
        *self.endpoint.lock() = Some(endpoint);
        *self.get_nick.write() = Some(get_nick);
        *self.get_tracks.write() = Some(get_tracks);

        let mgr = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut tick = 0u64;
            let mut interval = tokio::time::interval(Duration::from_millis(2500));
            while !mgr.stop.load(Ordering::SeqCst) {
                interval.tick().await;
                tick = tick.wrapping_add(1);
                let active_gid = mgr.active_group_id.read().clone();
                let sync_all = tick % 5 == 0;
                mgr.sync_groups_cycle(active_gid.as_deref(), sync_all).await;
            }
        });
    }

    pub fn drain_toasts(&self) -> Vec<(String, String)> {
        self.toasts.lock().drain(..).collect()
    }

    fn push_toast(&self, msg: String, icon: &str) {
        self.toasts.lock().push_back((msg, icon.to_string()));
    }

    /// Cria um novo grupo já com a sala padrão `# Geral` e coloca o criador como membro.
    pub fn create_group(&self, name: &str, my_nick: &str) -> Result<GroupView> {
        let clean = name.trim();
        if clean.is_empty() || clean.len() > 40 {
            anyhow::bail!("Escolha um nome entre 1 e 40 caracteres para o grupo.");
        }

        let mut secret = [0u8; 32];
        rand::rng().fill(&mut secret);
        let group_id = Uuid::new_v4().to_string();
        let now = now_unix_secs();

        let default_room = GroupRoom {
            id: "geral".to_string(),
            name: "Geral".to_string(),
            created_at: now,
        };

        let mut members = HashMap::new();
        members.insert(self.my_tag.clone(), self.my_member_record(my_nick));

        let record = GroupRecord {
            id: group_id.clone(),
            name: clean.to_string(),
            owner_tag: self.my_tag.clone(),
            secret_hex: hex::encode(secret),
            rooms: vec![default_room],
            members,
            created_at: now,
        };

        self.groups.write().insert(group_id.clone(), record);
        self.save();
        *self.active_group_id.write() = Some(group_id.clone());

        self.snapshot(&my_nick)
            .groups
            .into_iter()
            .find(|g| g.id == group_id)
            .ok_or_else(|| anyhow!("Falha ao construir visualização do grupo"))
    }

    pub fn create_room(self: &Arc<Self>, group_id: &str, room_name: &str) -> Result<GroupRoom> {
        let clean = room_name.trim().trim_start_matches('#').trim();
        if clean.is_empty() || clean.len() > 32 {
            anyhow::bail!("Nome da sala deve ter entre 1 e 32 caracteres.");
        }

        let room = GroupRoom {
            id: Uuid::new_v4().to_string()[..8].to_string(),
            name: clean.to_string(),
            created_at: now_unix_secs(),
        };

        {
            let mut lock = self.groups.write();
            let grp = lock
                .get_mut(group_id)
                .ok_or_else(|| anyhow!("Grupo não encontrado"))?;
            if grp.rooms.iter().any(|r| r.name.eq_ignore_ascii_case(clean)) {
                anyhow::bail!("Já existe uma sala chamada #{clean} neste grupo.");
            }
            grp.rooms.push(room.clone());
        }
        self.save();
        self.trigger_immediate_sync(group_id);
        Ok(room)
    }

    pub fn select_group(&self, group_id: Option<String>) {
        *self.active_group_id.write() = group_id;
    }

    pub fn enter_room(self: &Arc<Self>, group_id: &str, room_id: &str) -> Result<()> {
        {
            let lock = self.groups.read();
            let grp = lock
                .get(group_id)
                .ok_or_else(|| anyhow!("Grupo não encontrado"))?;
            if !grp.rooms.iter().any(|r| r.id == room_id) {
                anyhow::bail!("Sala não encontrada neste grupo");
            }
        }
        *self.active_group_id.write() = Some(group_id.to_string());
        *self.active_room_id.write() = Some(room_id.to_string());
        self.trigger_immediate_sync(group_id);
        Ok(())
    }

    pub fn exit_room(self: &Arc<Self>) {
        let prev_gid = self.active_group_id.read().clone();
        *self.active_room_id.write() = None;
        if let Some(gid) = prev_gid {
            self.trigger_immediate_sync(&gid);
        }
    }

    pub fn leave_group(&self, group_id: &str) -> Result<()> {
        let was_active = self.active_group_id.read().as_deref() == Some(group_id);
        if was_active {
            *self.active_room_id.write() = None;
            *self.active_group_id.write() = None;
        }
        self.groups.write().remove(group_id);
        self.presences.write().remove(group_id);
        self.save();
        Ok(())
    }

    pub fn trigger_immediate_sync(self: &Arc<Self>, group_id: &str) {
        if self.endpoint.lock().is_none() {
            return;
        }
        let mgr = self.clone();
        let gid = group_id.to_string();
        tauri::async_runtime::spawn(async move {
            mgr.sync_single_group(&gid).await;
        });
    }

    /// Gera um código curto `LKG-XXXX-XXXX` (Rendezvous P2P + Worker) + Ticket direto `lkg-...`.
    pub async fn create_group_invite(&self, group_id: &str, my_nick: &str) -> Result<GroupInviteBundle> {
        let snap = {
            let mut lock = self.groups.write();
            let grp = lock
                .get_mut(group_id)
                .ok_or_else(|| anyhow!("Grupo não encontrado"))?;
            grp.members.insert(self.my_tag.clone(), self.my_member_record(my_nick));
            GroupSnapshot::from(&*grp)
        };
        self.save();

        let direct_ticket = encode_direct_group_ticket(&snap)?;
        let code = generate_group_short_code();

        // Guarda convite ativo para resgate direto em 1 hop
        let rv_stop = Arc::new(AtomicBool::new(false));
        let keys = derive_group_code_keys(&code)?;
        let rv_secret = SecretKey::from_bytes(&keys.rv_secret_seed);
        let rv_stop_bg = rv_stop.clone();
        let host_ep = self.endpoint.lock().clone();

        if let Some(social_ep) = host_ep {
            let host_addr = social_ep.addr();
            let snap_clone = snap.clone();

            // Se Worker KV configurado, publica payload cifrado também para acesso offline imediato
            if let Some(worker_url) = configured_worker_url() {
                if let Ok(json_bytes) = serde_json::to_vec(&snap_clone) {
                    if let Ok(enc_blob) = encrypt_group_payload_for_code(&code, &json_bytes) {
                        let b58_body = bs58::encode(enc_blob).into_string();
                        let hash = keys.kv_blind_hash.clone();
                        tokio::spawn(async move {
                            if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(4)).build() {
                                let _ = client
                                    .put(format!("{worker_url}/v1/group/{hash}"))
                                    .body(b58_body)
                                    .send()
                                    .await;
                            }
                        });
                    }
                }
            }

            // Sobe nó Rendezvous Iroh nativo para entregar o EndpointAddr do anfitrião
            if let Ok(rv_ep) = Endpoint::builder(presets::N0)
                .secret_key(rv_secret)
                .alpns(vec![LUKE_GROUP_INVITE_RV_ALPN.to_vec()])
                .bind()
                .await
            {
                let _ = tokio::time::timeout(Duration::from_millis(900), rv_ep.online()).await;
                let code_clone = code.clone();
                tokio::spawn(async move {
                    let deadline = Instant::now() + Duration::from_secs(7200);
                    while !rv_stop_bg.load(Ordering::SeqCst) && Instant::now() < deadline {
                        let accepted = tokio::select! {
                            inc = rv_ep.accept() => inc,
                            _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                        };
                        let Some(incoming) = accepted else { break };
                        let code_c = code_clone.clone();
                        let addr_c = host_addr.clone();
                        tokio::spawn(async move {
                            let Ok(conn) = incoming.await else { return };
                            let Ok(mut send) = conn.open_uni().await else { return };
                            if let Ok(json) = serde_json::to_vec(&addr_c) {
                                if let Ok(blob) = encrypt_group_payload_for_code(&code_c, &json) {
                                    let _ = send.write_all(&blob).await;
                                    let _ = send.finish();
                                    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
                                }
                            }
                            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                        });
                    }
                    let _ = rv_ep.close().await;
                });
            }
        }

        self.active_invites.lock().insert(
            code.clone(),
            ActiveGroupInvite {
                group_id: group_id.to_string(),
                created_at: Instant::now(),
                rv_stop,
            },
        );

        Ok(GroupInviteBundle {
            short_code: Some(code),
            direct_ticket,
        })
    }

    pub async fn invite_friend_to_group(
        &self,
        group_id: &str,
        friend_pubkey_b58: &str,
        friend_addr: Option<EndpointAddr>,
        my_nick: &str,
    ) -> Result<()> {
        let ep = self
            .endpoint
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("Rede P2P ainda inicializando"))?;

        let snap = {
            let mut lock = self.groups.write();
            let grp = lock
                .get_mut(group_id)
                .ok_or_else(|| anyhow!("Grupo não encontrado"))?;
            grp.members.insert(self.my_tag.clone(), self.my_member_record(my_nick));
            GroupSnapshot::from(&*grp)
        };
        self.save();

        let remote_id = decode_pubkey_to_endpoint_id(friend_pubkey_b58)?;
        let conn = if let Some(hint) = friend_addr {
            match tokio::time::timeout(Duration::from_secs(5), ep.connect(hint, LUKE_GROUP_ALPN)).await {
                Ok(Ok(c)) => c,
                _ => tokio::time::timeout(
                    Duration::from_secs(7),
                    ep.connect(EndpointAddr::from(remote_id), LUKE_GROUP_ALPN),
                )
                .await
                .map_err(|_| anyhow!("Tempo esgotado conectando ao amigo para enviar convite"))??,
            }
        } else {
            tokio::time::timeout(
                Duration::from_secs(8),
                ep.connect(EndpointAddr::from(remote_id), LUKE_GROUP_ALPN),
            )
            .await
            .map_err(|_| anyhow!("Amigo offline no momento — envie o código LKG-XXXX-XXXX!"))??
        };

        let (mut send, mut recv) = conn.open_bi().await?;
        write_group_wire(
            &mut send,
            &GroupWire::InvitePush {
                from_tag: self.my_tag.clone(),
                from_nick: my_nick.to_string(),
                from_pubkey_b58: self.my_pubkey_b58.clone(),
                snapshot: snap,
                sender_addr: Some(ep.addr()),
            },
        )
        .await?;
        let _ = send.finish();

        match tokio::time::timeout(Duration::from_secs(5), read_group_wire(&mut recv)).await {
            Ok(Ok(GroupWire::InviteAck { ok: true })) => {
                conn.close(0u32.into(), b"ok");
                Ok(())
            }
            _ => {
                conn.close(0u32.into(), b"err");
                anyhow::bail!("Convite não confirmado pelo amigo");
            }
        }
    }

    pub fn respond_group_invite(self: &Arc<Self>, group_id: &str, accept: bool, my_nick: &str) -> Result<Option<GroupView>> {
        let invite_opt = {
            let mut lock = self.pending_invites.lock();
            lock.iter()
                .position(|i| i.group_id == group_id)
                .map(|idx| lock.remove(idx))
        };
        let Some(invite) = invite_opt else {
            return Ok(None);
        };
        if !accept {
            return Ok(None);
        }

        self.merge_snapshot(invite.snapshot, my_nick);
        *self.active_group_id.write() = Some(group_id.to_string());
        self.trigger_immediate_sync(group_id);

        Ok(self
            .snapshot(my_nick)
            .groups
            .into_iter()
            .find(|g| g.id == group_id))
    }

    pub async fn join_group_by_code(self: &Arc<Self>, raw_code: &str, my_nick: &str) -> Result<GroupView> {
        let trimmed = raw_code.trim();
        let snap = if let Some(code_norm) = normalize_group_short_code(trimmed) {
            let full_code = format!("LKG-{}-{}", &code_norm[..4], &code_norm[4..]);
            self.resolve_and_redeem_short_code(&full_code, my_nick).await?
        } else if trimmed.starts_with("lkg-") {
            decode_direct_group_ticket(trimmed)?
        } else {
            anyhow::bail!("Código inválido. Cole um código curto (LKG-XXXX-XXXX) ou ticket lkg-...");
        };

        let gid = snap.id.clone();
        self.merge_snapshot(snap, my_nick);
        *self.active_group_id.write() = Some(gid.clone());
        self.sync_single_group(&gid).await;

        self.snapshot(my_nick)
            .groups
            .into_iter()
            .find(|g| g.id == gid)
            .ok_or_else(|| anyhow!("Falha ao entrar no grupo"))
    }

    async fn resolve_and_redeem_short_code(&self, code: &str, my_nick: &str) -> Result<GroupSnapshot> {
        let keys = derive_group_code_keys(code)?;
        let ep = self
            .endpoint
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("Rede P2P ainda inicializando"))?;

        // 1. Tenta Rendezvous P2P Nativo primeiro
        let rv_id = SecretKey::from_bytes(&keys.rv_secret_seed).public();
        let host_addr_res = async {
            let conn = tokio::time::timeout(
                Duration::from_secs(7),
                ep.connect(rv_id, LUKE_GROUP_INVITE_RV_ALPN),
            )
            .await
            .map_err(|_| anyhow!("Tempo esgotado buscando código do grupo na rede P2P"))??;

            let mut recv = tokio::time::timeout(Duration::from_secs(4), conn.accept_uni())
                .await
                .map_err(|_| anyhow!("Timeout aguardando resposta de convite"))??;
            let blob = recv.read_to_end(32 * 1024).await?;
            conn.close(0u32.into(), b"ok");
            let json = decrypt_group_payload_for_code(code, &blob)?;
            let addr: EndpointAddr = serde_json::from_slice(&json)?;
            Ok::<EndpointAddr, anyhow::Error>(addr)
        }
        .await;

        if let Ok(host_addr) = host_addr_res {
            let conn = tokio::time::timeout(Duration::from_secs(6), ep.connect(host_addr, LUKE_GROUP_ALPN))
                .await
                .map_err(|_| anyhow!("Anfitrião do convite inacessível no momento"))??;

            let (mut send, mut recv) = conn.open_bi().await?;
            write_group_wire(
                &mut send,
                &GroupWire::RedeemInvite {
                    code: code.to_string(),
                    joiner: self.my_member_record(my_nick),
                    sender_addr: Some(ep.addr()),
                },
            )
            .await?;
            let _ = send.finish();

            if let Ok(Ok(GroupWire::RedeemAck { ok, snapshot, reason })) =
                tokio::time::timeout(Duration::from_secs(5), read_group_wire(&mut recv)).await
            {
                conn.close(0u32.into(), b"done");
                if ok {
                    if let Some(snap) = snapshot {
                        return Ok(snap);
                    }
                }
                anyhow::bail!("{}", reason.unwrap_or_else(|| "Convite expirado ou inválido".into()));
            }
        }

        // 2. Fallback: Cloudflare Worker KV (/v1/group/:hash) se configurado
        if let Some(worker_url) = configured_worker_url() {
            if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
                let url = format!("{worker_url}/v1/group/{}", keys.kv_blind_hash);
                if let Ok(resp) = client.get(url).send().await {
                    if resp.status().is_success() {
                        let b58 = resp.text().await.unwrap_or_default();
                        let blob = bs58::decode(b58.trim()).into_vec()?;
                        let json = decrypt_group_payload_for_code(code, &blob)?;
                        let snap: GroupSnapshot = serde_json::from_slice(&json)?;
                        return Ok(snap);
                    }
                }
            }
        }

        anyhow::bail!("Convite de grupo '{code}' não encontrado ou quem gerou o código ficou offline.");
    }

    pub fn merge_snapshot(&self, snap: GroupSnapshot, my_nick: &str) {
        {
            let mut lock = self.groups.write();
            let entry = lock.entry(snap.id.clone()).or_insert_with(|| GroupRecord {
                id: snap.id.clone(),
                name: snap.name.clone(),
                owner_tag: snap.owner_tag.clone(),
                secret_hex: snap.secret_hex.clone(),
                rooms: snap.rooms.clone(),
                members: HashMap::new(),
                created_at: snap.created_at,
            });
            for r in snap.rooms {
                if !entry.rooms.iter().any(|cur| cur.id == r.id) {
                    entry.rooms.push(r);
                }
            }
            for m in snap.members {
                entry
                    .members
                    .entry(m.tag.clone())
                    .and_modify(|cur| {
                        cur.nick = m.nick.clone();
                        if m.last_addr.is_some() {
                            cur.last_addr = m.last_addr.clone();
                        }
                    })
                    .or_insert(m);
            }
            entry
                .members
                .insert(self.my_tag.clone(), self.my_member_record(my_nick));
        }
        self.save();
    }

    fn apply_presence(&self, group_id: &str, p: RoomPresence) {
        let mut lock = self.presences.write();
        let gmap = lock.entry(group_id.to_string()).or_default();
        if p.room_id.is_none() {
            gmap.remove(&p.tag);
        } else {
            gmap.insert(
                p.tag.clone(),
                LivePresenceEntry {
                    presence: p,
                    received_at: Instant::now(),
                },
            );
        }
    }

    fn build_my_presence(&self, group_id: &str, my_nick: &str) -> RoomPresence {
        let active_gid = self.active_group_id.read().clone();
        let active_rid = self.active_room_id.read().clone();
        let room_id = if active_gid.as_deref() == Some(group_id) {
            active_rid
        } else {
            None
        };
        let (screen_ticket, camera_ticket) = if room_id.is_some() {
            self.current_tracks()
        } else {
            (None, None)
        };
        RoomPresence {
            tag: self.my_tag.clone(),
            nick: my_nick.to_string(),
            room_id,
            screen_ticket,
            camera_ticket,
            updated_at_ms: now_unix_ms(),
        }
    }

    pub async fn sync_groups_cycle(self: &Arc<Self>, priority_group_id: Option<&str>, sync_all: bool) {
        let gids: Vec<String> = {
            let lock = self.groups.read();
            if sync_all {
                lock.keys().cloned().collect()
            } else if let Some(pg) = priority_group_id {
                if lock.contains_key(pg) {
                    vec![pg.to_string()]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        };

        for gid in gids {
            self.sync_single_group(&gid).await;
        }
    }

    pub async fn sync_single_group(self: &Arc<Self>, group_id: &str) {
        let Some(ep) = self.endpoint.lock().clone() else {
            return;
        };
        let my_nick = self.current_nick();
        let my_presence = self.build_my_presence(group_id, &my_nick);
        let my_member = self.my_member_record(&my_nick);

        let (grp_snap, peers): (GroupSnapshot, Vec<GroupMember>) = {
            let lock = self.groups.read();
            let Some(grp) = lock.get(group_id) else {
                return;
            };
            let snap = GroupSnapshot::from(grp);
            let peers = grp
                .members
                .values()
                .filter(|m| m.tag != self.my_tag)
                .cloned()
                .collect();
            (snap, peers)
        };

        let my_addr = ep.addr();
        let futs = peers.into_iter().map(|peer| {
            let mgr_self = self.clone();
            let ep_c = ep.clone();
            let snap_c = grp_snap.clone();
            let my_m = my_member.clone();
            let my_p = my_presence.clone();
            let my_addr_c = my_addr.clone();
            async move {
                let Ok(remote_id) = decode_pubkey_to_endpoint_id(&peer.pubkey_b58) else {
                    return;
                };
                let conn_res = if let Some(hint) = peer.last_addr {
                    match tokio::time::timeout(Duration::from_secs(2), ep_c.connect(hint, LUKE_GROUP_ALPN)).await {
                        Ok(Ok(c)) => Ok(c),
                        _ => tokio::time::timeout(
                            Duration::from_secs(3),
                            ep_c.connect(EndpointAddr::from(remote_id), LUKE_GROUP_ALPN),
                        )
                        .await
                        .map_err(|_| anyhow!("timeout"))
                        .and_then(|r| r.map_err(|e| anyhow!("{e}"))),
                    }
                } else {
                    tokio::time::timeout(
                        Duration::from_secs(3),
                        ep_c.connect(EndpointAddr::from(remote_id), LUKE_GROUP_ALPN),
                    )
                    .await
                    .map_err(|_| anyhow!("timeout"))
                    .and_then(|r| r.map_err(|e| anyhow!("{e}")))
                };

                let Ok(conn) = conn_res else { return };
                let Ok((mut send, mut recv)) = conn.open_bi().await else { return };
                let wire = GroupWire::Sync {
                    group_id: snap_c.id.clone(),
                    group_name: snap_c.name.clone(),
                    owner_tag: snap_c.owner_tag.clone(),
                    secret_hex: snap_c.secret_hex.clone(),
                    rooms: snap_c.rooms.clone(),
                    members: snap_c.members.clone(),
                    from_member: my_m,
                    presence: Some(my_p),
                    sender_addr: Some(my_addr_c),
                };
                if write_group_wire(&mut send, &wire).await.is_err() {
                    return;
                }
                let _ = send.finish();

                if let Ok(Ok(GroupWire::SyncAck {
                    ok: true,
                    rooms,
                    members,
                    presences,
                    responder_addr,
                })) = tokio::time::timeout(Duration::from_secs(3), read_group_wire(&mut recv)).await
                {
                    let ack_snap = GroupSnapshot {
                        id: snap_c.id.clone(),
                        name: snap_c.name.clone(),
                        owner_tag: snap_c.owner_tag.clone(),
                        secret_hex: snap_c.secret_hex.clone(),
                        rooms,
                        members,
                        created_at: snap_c.created_at,
                    };
                    mgr_self.merge_snapshot(ack_snap, &mgr_self.current_nick());
                    if let Some(addr) = responder_addr {
                        if let Some(g) = mgr_self.groups.write().get_mut(&snap_c.id) {
                            if let Some(m) = g.members.get_mut(&peer.tag) {
                                m.last_addr = Some(addr);
                            }
                        }
                    }
                    for p in presences {
                        if p.tag != mgr_self.my_tag {
                            mgr_self.apply_presence(&snap_c.id, p);
                        }
                    }
                }
                conn.close(0u32.into(), b"sync");
            }
        });

        let _ = futures_join_all_bounded(futs).await;
    }

    pub async fn handle_incoming_conn(&self, conn: iroh::endpoint::Connection) {
        let remote_pk_bytes = *conn.remote_id().as_bytes();
        let remote_b58 = bs58::encode(remote_pk_bytes).into_string();
        let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
        let Ok(wire) = read_group_wire(&mut recv).await else { return };

        match wire {
            GroupWire::Sync {
                group_id,
                group_name,
                owner_tag,
                secret_hex,
                rooms,
                members,
                from_member,
                presence,
                sender_addr,
            } => {
                // Verifica criptograficamente que a identidade QUIC bate com o membro remetente
                if from_member.pubkey_b58 != remote_b58 {
                    let _ = write_group_wire(
                        &mut send,
                        &GroupWire::SyncAck {
                            ok: false,
                            rooms: Vec::new(),
                            members: Vec::new(),
                            presences: Vec::new(),
                            responder_addr: None,
                        },
                    )
                    .await;
                    let _ = send.finish();
                    return;
                }

                let is_member_or_matching_secret = {
                    let lock = self.groups.read();
                    lock.get(&group_id)
                        .map(|g| g.secret_hex == secret_hex && g.members.contains_key(&from_member.tag) || members.iter().any(|m| m.tag == from_member.tag) && g.secret_hex == secret_hex)
                        .unwrap_or(false)
                };

                if !is_member_or_matching_secret {
                    let _ = write_group_wire(
                        &mut send,
                        &GroupWire::SyncAck {
                            ok: false,
                            rooms: Vec::new(),
                            members: Vec::new(),
                            presences: Vec::new(),
                            responder_addr: None,
                        },
                    )
                    .await;
                    let _ = send.finish();
                    return;
                }

                let mut peer_member = from_member.clone();
                if sender_addr.is_some() {
                    peer_member.last_addr = sender_addr;
                }
                let mut merged_members = members;
                merged_members.push(peer_member);

                let my_nick = self.current_nick();
                self.merge_snapshot(
                    GroupSnapshot {
                        id: group_id.clone(),
                        name: group_name,
                        owner_tag,
                        secret_hex,
                        rooms,
                        members: merged_members,
                        created_at: now_unix_secs(),
                    },
                    &my_nick,
                );

                if let Some(p) = presence {
                    if p.tag == from_member.tag {
                        self.apply_presence(&group_id, p);
                    }
                }

                let (ack_rooms, ack_members) = {
                    let lock = self.groups.read();
                    if let Some(g) = lock.get(&group_id) {
                        (g.rooms.clone(), g.members.values().cloned().collect())
                    } else {
                        (Vec::new(), Vec::new())
                    }
                };
                let mut ack_presences = self.collect_live_presences(&group_id);
                let my_p = self.build_my_presence(&group_id, &my_nick);
                if my_p.room_id.is_some() {
                    ack_presences.push(my_p);
                }

                let resp_addr = { self.endpoint.lock().as_ref().map(|e| e.addr()) };
                let _ = write_group_wire(
                    &mut send,
                    &GroupWire::SyncAck {
                        ok: true,
                        rooms: ack_rooms,
                        members: ack_members,
                        presences: ack_presences,
                        responder_addr: resp_addr,
                    },
                )
                .await;
            }
            GroupWire::InvitePush {
                from_tag,
                from_nick,
                from_pubkey_b58,
                mut snapshot,
                sender_addr,
            } => {
                if from_pubkey_b58 != remote_b58 {
                    let _ = write_group_wire(&mut send, &GroupWire::InviteAck { ok: false }).await;
                    let _ = send.finish();
                    return;
                }
                if let Some(addr) = sender_addr {
                    for m in &mut snapshot.members {
                        if m.tag == from_tag {
                            m.last_addr = Some(addr.clone());
                        }
                    }
                }
                let already_joined = self.groups.read().contains_key(&snapshot.id);
                if already_joined {
                    self.merge_snapshot(snapshot, &self.current_nick());
                } else {
                    let mut lock = self.pending_invites.lock();
                    lock.retain(|i| i.group_id != snapshot.id);
                    lock.push(PendingGroupInvite {
                        group_id: snapshot.id.clone(),
                        group_name: snapshot.name.clone(),
                        from_tag: from_tag.clone(),
                        from_nick: from_nick.clone(),
                        snapshot: snapshot.clone(),
                    });
                    self.push_toast(
                        format!("{from_nick} convidou você para o grupo \"{}\"", snapshot.name),
                        "groups",
                    );
                }
                let _ = write_group_wire(&mut send, &GroupWire::InviteAck { ok: true }).await;
            }
            GroupWire::RedeemInvite {
                code,
                mut joiner,
                sender_addr,
            } => {
                if joiner.pubkey_b58 != remote_b58 {
                    let _ = write_group_wire(
                        &mut send,
                        &GroupWire::RedeemAck {
                            ok: false,
                            snapshot: None,
                            reason: Some("Assinatura QUIC não corresponde à chave do convidado".into()),
                        },
                    )
                    .await;
                    let _ = send.finish();
                    return;
                }
                let gid_opt = self
                    .active_invites
                    .lock()
                    .get(code.trim())
                    .map(|a| a.group_id.clone());
                let Some(gid) = gid_opt else {
                    let _ = write_group_wire(
                        &mut send,
                        &GroupWire::RedeemAck {
                            ok: false,
                            snapshot: None,
                            reason: Some("Código de convite expirado ou inválido".into()),
                        },
                    )
                    .await;
                    let _ = send.finish();
                    return;
                };

                if sender_addr.is_some() {
                    joiner.last_addr = sender_addr;
                }
                let snap_opt = {
                    let mut lock = self.groups.write();
                    if let Some(grp) = lock.get_mut(&gid) {
                        grp.members.insert(joiner.tag.clone(), joiner.clone());
                        Some(GroupSnapshot::from(&*grp))
                    } else {
                        None
                    }
                };
                if snap_opt.is_some() {
                    self.save();
                    self.push_toast(format!("{} entrou no grupo via convite!", joiner.nick), "group_add");
                }
                let _ = write_group_wire(
                    &mut send,
                    &GroupWire::RedeemAck {
                        ok: snap_opt.is_some(),
                        snapshot: snap_opt,
                        reason: None,
                    },
                )
                .await;
            }
            _ => {}
        }

        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    }

    fn collect_live_presences(&self, group_id: &str) -> Vec<RoomPresence> {
        let mut lock = self.presences.write();
        let Some(gmap) = lock.get_mut(group_id) else {
            return Vec::new();
        };
        gmap.retain(|_, v| v.received_at.elapsed() <= Duration::from_secs(PRESENCE_TTL_SECS));
        gmap.values().map(|v| v.presence.clone()).collect()
    }

    pub fn snapshot(&self, my_nick: &str) -> GroupsStateSnapshot {
        let active_gid = self.active_group_id.read().clone();
        let active_rid = self.active_room_id.read().clone();
        let (my_screen_ticket, my_camera_ticket) = self.current_tracks();

        let grp_list: Vec<GroupRecord> = {
            let mut list: Vec<GroupRecord> = self.groups.read().values().cloned().collect();
            list.sort_by_key(|g| g.created_at);
            list
        };

        let mut group_views = Vec::with_capacity(grp_list.len());
        for g in grp_list {
            let live_peers = self.collect_live_presences(&g.id);
            let mut online_tags: std::collections::HashSet<String> =
                live_peers.iter().map(|p| p.tag.clone()).collect();
            online_tags.insert(self.my_tag.clone());

            let mut room_views = Vec::with_capacity(g.rooms.len());
            for r in &g.rooms {
                let mut participants = Vec::new();

                if active_gid.as_deref() == Some(&g.id) && active_rid.as_deref() == Some(&r.id) {
                    participants.push(ParticipantView {
                        tag: self.my_tag.clone(),
                        nick: my_nick.to_string(),
                        is_self: true,
                        screen_ticket: my_screen_ticket.clone(),
                        camera_ticket: my_camera_ticket.clone(),
                        has_screen: my_screen_ticket.is_some(),
                        has_camera: my_camera_ticket.is_some(),
                    });
                }

                for lp in &live_peers {
                    if lp.tag != self.my_tag && lp.room_id.as_deref() == Some(&r.id) {
                        participants.push(ParticipantView {
                            tag: lp.tag.clone(),
                            nick: lp.nick.clone(),
                            is_self: false,
                            screen_ticket: lp.screen_ticket.clone(),
                            camera_ticket: lp.camera_ticket.clone(),
                            has_screen: lp.screen_ticket.is_some(),
                            has_camera: lp.camera_ticket.is_some(),
                        });
                    }
                }

                room_views.push(GroupRoomView {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    participants,
                });
            }

            group_views.push(GroupView {
                id: g.id.clone(),
                name: g.name.clone(),
                owner_tag: g.owner_tag.clone(),
                is_owner: g.owner_tag == self.my_tag,
                rooms: room_views,
                members_count: g.members.len(),
                online_members_count: online_tags.len(),
            });
        }

        let pending_invites = self
            .pending_invites
            .lock()
            .iter()
            .map(|p| PendingGroupInviteSummary {
                group_id: p.group_id.clone(),
                group_name: p.group_name.clone(),
                from_tag: p.from_tag.clone(),
                from_nick: p.from_nick.clone(),
                rooms_count: p.snapshot.rooms.len(),
                members_count: p.snapshot.members.len(),
            })
            .collect();

        GroupsStateSnapshot {
            groups: group_views,
            active_group_id: active_gid,
            active_room_id: active_rid,
            pending_invites,
        }
    }
}

async fn futures_join_all_bounded<I, F>(iter: I) -> Vec<()>
where
    I: IntoIterator<Item = F>,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut handles = Vec::new();
    for fut in iter {
        handles.push(tokio::spawn(fut));
    }
    for h in handles {
        let _ = h.await;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_ticket_and_short_code_roundtrip() -> Result<()> {
        let mgr = GroupManager::ephemeral_for_test();
        let g = mgr.create_group("Time Alpha", "Lucas")?;
        let snap = {
            let lock = mgr.groups.read();
            GroupSnapshot::from(lock.get(&g.id).unwrap())
        };
        let encoded = encode_direct_group_ticket(&snap)?;
        assert!(encoded.starts_with("lkg-"));
        let decoded = decode_direct_group_ticket(&encoded)?;
        assert_eq!(decoded.id, snap.id);
        assert_eq!(decoded.name, "Time Alpha");
        assert_eq!(decoded.rooms.len(), 1);
        assert_eq!(decoded.rooms[0].name, "Geral");

        let code = generate_group_short_code();
        assert!(normalize_group_short_code(&code).is_some());
        let blob = encrypt_group_payload_for_code(&code, b"hello-group")?;
        let plain = decrypt_group_payload_for_code(&code, &blob)?;
        assert_eq!(plain, b"hello-group");
        Ok(())
    }

    #[tokio::test]
    async fn p2p_group_sync_shares_screen_and_webcam_presence() -> Result<()> {
        let alice = GroupManager::ephemeral_for_test();
        let bob = GroupManager::ephemeral_for_test();

        let ep_a = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&alice.signing_key.to_bytes()))
            .alpns(vec![LUKE_GROUP_ALPN.to_vec()])
            .bind()
            .await?;
        let ep_b = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&bob.signing_key.to_bytes()))
            .alpns(vec![LUKE_GROUP_ALPN.to_vec()])
            .bind()
            .await?;

        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(ep_a.online(), ep_b.online());
        })
        .await;

        *alice.endpoint.lock() = Some(ep_a.clone());
        *bob.endpoint.lock() = Some(ep_b.clone());
        *alice.get_tracks.write() = Some(Arc::new(|| {
            (
                Some("lk-screen-alice".to_string()),
                Some("lk-cam-alice".to_string()),
            )
        }));
        *bob.get_tracks.write() = Some(Arc::new(|| (None, None)));

        // Aceita conexões no nó da Alice
        let alice_srv = alice.clone();
        let ep_a_srv = ep_a.clone();
        let srv_handle = tokio::spawn(async move {
            if let Some(inc) = ep_a_srv.accept().await {
                if let Ok(conn) = inc.await {
                    alice_srv.handle_incoming_conn(conn).await;
                }
            }
        });

        let grp = alice.create_group("Squad Dev", "Alice")?;
        let room_id = grp.rooms[0].id.clone();
        alice.enter_room(&grp.id, &room_id)?;

        let mut snap = {
            let lock = alice.groups.read();
            GroupSnapshot::from(lock.get(&grp.id).unwrap())
        };
        for m in &mut snap.members {
            if m.tag == alice.my_tag {
                m.last_addr = Some(ep_a.addr());
            }
        }

        bob.merge_snapshot(snap, "Bob");
        bob.enter_room(&grp.id, &room_id)?;
        bob.sync_single_group(&grp.id).await;

        let bob_view = bob.snapshot("Bob");
        let g_bob = bob_view.groups.iter().find(|g| g.id == grp.id).unwrap();
        let p_alice = g_bob.rooms[0]
            .participants
            .iter()
            .find(|p| p.tag == alice.my_tag)
            .expect("Alice deve aparecer presente na sala do Bob");
        assert!(p_alice.has_screen);
        assert!(p_alice.has_camera);
        assert_eq!(p_alice.screen_ticket.as_deref(), Some("lk-screen-alice"));
        assert_eq!(p_alice.camera_ticket.as_deref(), Some("lk-cam-alice"));

        let _ = tokio::time::timeout(Duration::from_secs(2), srv_handle).await;
        let _ = ep_a.close().await;
        let _ = ep_b.close().await;
        Ok(())
    }
}
