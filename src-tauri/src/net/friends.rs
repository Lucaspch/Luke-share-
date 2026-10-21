//! Gerenciamento de Identidade Criptográfica (Ed25519) e Amigos P2P.
//!
//! Cada computador gera e armazena com segurança seu par de chaves em %APPDATA%/Luke-Share/identity.key.
//! A partir da Chave Pública, derivamos uma Tag única: `LK#XXXX` (ex: LK#7M2Q).
//! Amigos são salvos em %APPDATA%/Luke-Share/friends.json e sincronizam status e pedidos via envelopes autenticados.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LUKE_IDENTITY_SALT: &[u8] = b"LUKE-SHARE-ED25519-IDENTITY-SALT-2026";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct IdentityInfo {
    pub tag: String,
    pub public_key_b58: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum FriendStatus {
    PendingOut, // Você enviou pedido
    PendingIn,  // Ele te enviou pedido
    Accepted,   // Amigos confirmados mutuamente
    Blocked,    // Bloqueado
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Friend {
    pub tag: String,
    pub nick: String,
    pub public_key_b58: String,
    pub status: FriendStatus,
    pub is_online: bool,
    pub is_streaming: bool,
    pub room_code: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FriendEnvelope {
    pub from_tag: String,
    pub from_nick: String,
    pub from_pubkey_b58: String,
    pub to_tag: String,
    pub action: String, // "friend_request" | "friend_accept" | "friend_reject" | "presence" | "knock" | "knock_accept"
    pub payload: Option<String>,
    pub signature_hex: String,
}

pub struct FriendsManager {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    pub tag: String,
    pub friends: Arc<RwLock<HashMap<String, Friend>>>,
}

fn get_data_dir() -> Result<PathBuf> {
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("LOCALAPPDATA"))
        .context("Variável de ambiente APPDATA não encontrada")?;
    let mut dir = PathBuf::from(base);
    dir.push("Luke-Share");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn derive_tag(pubkey: &VerifyingKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(LUKE_IDENTITY_SALT);
    hasher.update(pubkey.as_bytes());
    let hash = hasher.finalize();

    let charset = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut tag_suffix = String::with_capacity(4);
    for i in 0..4 {
        let idx = (hash[i] as usize) % charset.len();
        tag_suffix.push(charset[idx] as char);
    }
    format!("LK#{}", tag_suffix)
}

impl FriendsManager {
    pub fn load_or_create() -> Result<Self> {
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
                        // Ao iniciar, status de rede é resetado
                        f.is_online = false;
                        f.is_streaming = false;
                        f.room_code = None;
                        friends_map.insert(f.tag.clone(), f);
                    }
                }
            }
        }

        Ok(Self {
            signing_key,
            verifying_key,
            tag,
            friends: Arc::new(RwLock::new(friends_map)),
        })
    }

    pub fn public_key_b58(&self) -> String {
        bs58::encode(self.verifying_key.as_bytes()).into_string()
    }

    pub fn save_friends(&self) {
        if let Ok(dir) = get_data_dir() {
            let friends_path = dir.join("friends.json");
            let list: Vec<Friend> = self.friends.read().values().cloned().collect();
            if let Ok(json) = serde_json::to_string_pretty(&list) {
                let _ = fs::write(friends_path, json);
            }
        }
    }

    pub fn list_friends(&self) -> Vec<Friend> {
        self.friends.read().values().cloned().collect()
    }

    pub fn add_or_request_friend(&self, target_tag: &str, my_nick: &str) -> Result<FriendEnvelope> {
        let clean_tag = target_tag.trim().to_uppercase();
        if clean_tag == self.tag {
            anyhow::bail!("Você não pode adicionar a sua própria tag!");
        }

        let envelope = self.sign_envelope(&clean_tag, my_nick, "friend_request", None)?;

        let mut lock = self.friends.write();
        lock.insert(
            clean_tag.clone(),
            Friend {
                tag: clean_tag,
                nick: "Amigo Pendente".into(),
                public_key_b58: "".into(),
                status: FriendStatus::PendingOut,
                is_online: false,
                is_streaming: false,
                room_code: None,
            },
        );
        drop(lock);
        self.save_friends();

        Ok(envelope)
    }

    pub fn accept_friend(&self, friend_tag: &str, my_nick: &str) -> Result<FriendEnvelope> {
        let clean_tag = friend_tag.trim().to_uppercase();
        let mut lock = self.friends.write();
        let f = lock
            .get_mut(&clean_tag)
            .ok_or_else(|| anyhow!("Solicitação não encontrada para {}", clean_tag))?;

        f.status = FriendStatus::Accepted;
        drop(lock);
        self.save_friends();

        self.sign_envelope(&clean_tag, my_nick, "friend_accept", None)
    }

    pub fn reject_friend(&self, friend_tag: &str, my_nick: &str) -> Result<FriendEnvelope> {
        let clean_tag = friend_tag.trim().to_uppercase();
        let mut lock = self.friends.write();
        lock.remove(&clean_tag);
        drop(lock);
        self.save_friends();

        self.sign_envelope(&clean_tag, my_nick, "friend_reject", None)
    }

    pub fn remove_friend(&self, friend_tag: &str) -> Result<()> {
        let clean_tag = friend_tag.trim().to_uppercase();
        self.friends.write().remove(&clean_tag);
        self.save_friends();
        Ok(())
    }

    pub fn sign_envelope(
        &self,
        to_tag: &str,
        from_nick: &str,
        action: &str,
        payload: Option<String>,
    ) -> Result<FriendEnvelope> {
        let mut msg_bytes = Vec::new();
        msg_bytes.extend_from_slice(self.tag.as_bytes());
        msg_bytes.extend_from_slice(to_tag.as_bytes());
        msg_bytes.extend_from_slice(action.as_bytes());
        if let Some(ref p) = payload {
            msg_bytes.extend_from_slice(p.as_bytes());
        }

        // Assinatura com a chave privada permanente
        let sig = self.signing_key.sign(&msg_bytes);
        let signature_hex = hex::encode(sig.to_bytes());

        Ok(FriendEnvelope {
            from_tag: self.tag.clone(),
            from_nick: from_nick.to_string(),
            from_pubkey_b58: self.public_key_b58(),
            to_tag: to_tag.to_string(),
            action: action.to_string(),
            payload,
            signature_hex,
        })
    }

    pub fn poll_topic_for_tag(tag: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"LK-FRIEND-INBOX-");
        hasher.update(tag.trim().to_uppercase().as_bytes());
        let hash = hasher.finalize();
        format!("lk_box_{}", hex::encode(&hash[..10]))
    }

    pub async fn send_envelope_to_cloud(envelope: &FriendEnvelope) -> Result<()> {
        let topic = Self::poll_topic_for_tag(&envelope.to_tag);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;

        let json = serde_json::to_string(envelope)?;
        let url = format!("https://ntfy.sh/{}", topic);
        let res = client.post(&url).body(json).send().await?;

        if !res.status().is_success() {
            anyhow::bail!("Falha ao despachar envelope para amigo (HTTP {})", res.status());
        }
        Ok(())
    }

    pub async fn poll_inbox_from_cloud(&self) -> Result<Vec<FriendEnvelope>> {
        let topic = Self::poll_topic_for_tag(&self.tag);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(6))
            .build()?;

        let url = format!("https://ntfy.sh/{}/json?poll=1", topic);
        let res = client.get(&url).send().await?;
        if !res.status().is_success() {
            return Ok(Vec::new());
        }

        let body = res.text().await?;
        let mut envelopes = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(msg_str) = val.get("message").and_then(|m| m.as_str()) {
                    if let Ok(env) = serde_json::from_str::<FriendEnvelope>(msg_str) {
                        envelopes.push(env);
                    }
                }
            }
        }

        Ok(envelopes)
    }

    pub fn verify_and_process_envelope(&self, env: &FriendEnvelope) -> Result<Option<String>> {
        if env.to_tag != self.tag {
            return Ok(None);
        }

        // 1. Valida chave pública
        let pubkey_bytes = bs58::decode(&env.from_pubkey_b58).into_vec()?;
        if pubkey_bytes.len() != 32 {
            anyhow::bail!("Chave pública do remetente inválida");
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&pubkey_bytes);
        let peer_pubkey = VerifyingKey::from_bytes(&arr)?;

        // Valida se a tag bate exatamente com a chave
        let expected_tag = derive_tag(&peer_pubkey);
        if expected_tag != env.from_tag {
            anyhow::bail!("Tag do remetente forjada ou não confere com a chave pública!");
        }

        // 2. Valida assinatura digital
        let mut msg_bytes = Vec::new();
        msg_bytes.extend_from_slice(env.from_tag.as_bytes());
        msg_bytes.extend_from_slice(env.to_tag.as_bytes());
        msg_bytes.extend_from_slice(env.action.as_bytes());
        if let Some(ref p) = env.payload {
            msg_bytes.extend_from_slice(p.as_bytes());
        }

        let sig_bytes = hex::decode(&env.signature_hex)?;
        if sig_bytes.len() != 64 {
            anyhow::bail!("Assinatura digital corrompida");
        }
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);

        peer_pubkey
            .verify_strict(&msg_bytes, &signature)
            .map_err(|e| anyhow!("Assinatura inválida: {e:#}"))?;

        // 3. Processa a ação
        let mut changed = false;
        let mut notify_event = None;

        match env.action.as_str() {
            "friend_request" => {
                let mut lock = self.friends.write();
                let existing = lock.get_mut(&env.from_tag);
                if let Some(f) = existing {
                    if f.status == FriendStatus::PendingOut {
                        // Ambos enviaram pedido -> Aceite mútuo automático!
                        f.status = FriendStatus::Accepted;
                        f.nick = env.from_nick.clone();
                        f.public_key_b58 = env.from_pubkey_b58.clone();
                        changed = true;
                        notify_event = Some(format!("Amizade confirmada com {}!", env.from_nick));
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
                            room_code: None,
                        },
                    );
                    changed = true;
                    notify_event = Some(format!("{} enviou uma solicitação de amizade!", env.from_nick));
                }
            }
            "friend_accept" => {
                let mut lock = self.friends.write();
                if let Some(f) = lock.get_mut(&env.from_tag) {
                    f.status = FriendStatus::Accepted;
                    f.nick = env.from_nick.clone();
                    f.public_key_b58 = env.from_pubkey_b58.clone();
                    f.is_online = true;
                    changed = true;
                    notify_event = Some(format!("{} aceitou seu pedido de amizade!", env.from_nick));
                }
            }
            "friend_reject" => {
                let mut lock = self.friends.write();
                if lock.remove(&env.from_tag).is_some() {
                    changed = true;
                }
            }
            "presence" => {
                let mut lock = self.friends.write();
                if let Some(f) = lock.get_mut(&env.from_tag) {
                    if f.status == FriendStatus::Accepted {
                        f.is_online = true;
                        if let Some(ref room) = env.payload {
                            f.is_streaming = !room.is_empty();
                            f.room_code = if room.is_empty() { None } else { Some(room.clone()) };
                        } else {
                            f.is_streaming = false;
                            f.room_code = None;
                        }
                    }
                }
            }
            _ => {}
        }

        if changed {
            self.save_friends();
        }

        Ok(notify_event)
    }
}
