//! Códigos Curtos Blindados (`LK-XXXX-XXXX`) com HKDF-SHA256 + AES-256-GCM.
//!
//! Arquitetura Híbrida Zero-Knowledge:
//! 1. O `RoomTicket` (`EndpointAddr`) é comprimido e criptografado localmente com AES-256-GCM
//!    usando chave derivada via HKDF-SHA256 (código de 8 caracteres + segredo interno do protocolo).
//! 2. Publicação dupla:
//!    - **Rendezvous P2P Nativo Iroh (`dns.iroh.link` / Pkarr)**: funciona imediatamente sem conta externa.
//!    - **Cloudflare Worker KV (opcional, TTL automático de 2h)**: quando configurado em `worker_url.txt`
//!      ou `LUKE_WORKER_URL`, armazena apenas o ciphertext opaco sob hash cego SHA-256.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Context, Result};
use hmac::{Hmac, Mac};
use iroh::{endpoint::presets, Endpoint, EndpointAddr, SecretKey};
use rand::RngExt;
use sha2::Sha256;

use super::p2p::RoomTicket;

pub const LUKE_RV_ALPN: &[u8] = b"luke-share-rv/1.0";
const LUKE_SHORTCODE_IKM: &[u8] = b"LUKE-SHARE-V05-HKDF-MASTER-PROTOCOL-KEY-2026";
const LUKE_SHORTCODE_AAD: &[u8] = b"luke-share-v0.5-room-aead";

/// Alfabeto de 32 caracteres legíveis (sem ambiguidade entre 0/O e 1/I).
pub const SAFE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

type HmacSha256 = Hmac<Sha256>;

/// Retorna URL opcional do Cloudflare Worker (variável `LUKE_WORKER_URL` ou `%APPDATA%/Luke-Share/worker_url.txt`).
pub fn configured_worker_url() -> Option<String> {
    if let Ok(v) = std::env::var("LUKE_WORKER_URL") {
        let t = v.trim().trim_end_matches('/').to_string();
        if t.starts_with("http://") || t.starts_with("https://") {
            return Some(t);
        }
    }
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("LOCALAPPDATA"))
        .ok()?;
    let mut path = std::path::PathBuf::from(base);
    path.push("Luke-Share");
    path.push("worker_url.txt");
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().trim_end_matches('/').to_string();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed)
    } else {
        None
    }
}

/// Gera um código curto aleatório no formato `LK-XXXX-XXXX` sem caracteres ambíguos.
pub fn generate_short_code() -> String {
    let mut rng = rand::rng();
    let mut rand_bytes = [0u8; 8];
    rng.fill(&mut rand_bytes);

    let mut chars = [0u8; 8];
    for i in 0..8 {
        chars[i] = SAFE_ALPHABET[(rand_bytes[i] as usize) & 31];
    }
    let part1 = std::str::from_utf8(&chars[0..4]).unwrap_or("ABCD");
    let part2 = std::str::from_utf8(&chars[4..8]).unwrap_or("EFGH");
    format!("LK-{part1}-{part2}")
}

/// Normaliza uma entrada do usuário para os 8 caracteres puros do código curto, se válido.
/// Aceita `LK-8F2A-9K4M`, `lk-8f2a-9k4m`, `8F2A-9K4M` ou `8F2A9K4M`.
/// Rejeita tickets completos `lk-eyJ...` (que são JSON Base64 longos).
pub fn normalize_short_code(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.len() > 20 {
        return None;
    }
    // Tickets longos base64 começam com "lk-ey"
    if trimmed.starts_with("lk-ey") {
        return None;
    }

    let upper_alnum: String = trimmed
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();

    let core = if upper_alnum.len() == 10 && upper_alnum.starts_with("LK") {
        &upper_alnum[2..]
    } else if upper_alnum.len() == 8 {
        &upper_alnum[..]
    } else {
        return None;
    };

    if core.chars().all(|c| SAFE_ALPHABET.contains(&(c as u8))) {
        Some(core.to_string())
    } else {
        None
    }
}

pub fn is_short_code(input: &str) -> bool {
    normalize_short_code(input).is_some()
}

pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(salt).expect("HMAC key");
    mac.update(ikm);
    mac.finalize().into_bytes().into()
}

pub fn hkdf_expand_32(prk: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(prk).expect("HMAC PRK");
    mac.update(info);
    mac.update(&[1u8]);
    mac.finalize().into_bytes().into()
}

pub struct DerivedShortCodeKeys {
    pub aes_key: [u8; 32],
    pub kv_blind_hash: String,
    pub rv_secret_seed: [u8; 32],
}

pub fn derive_short_code_keys(code: &str) -> Result<DerivedShortCodeKeys> {
    let core = normalize_short_code(code)
        .ok_or_else(|| anyhow!("Formato de código curto inválido (esperado LK-XXXX-XXXX)"))?;

    let prk = hkdf_extract(LUKE_SHORTCODE_IKM, core.as_bytes());
    let aes_key = hkdf_expand_32(&prk, b"luke-share/v0.5/aes-256-gcm-key");
    let kv_raw = hkdf_expand_32(&prk, b"luke-share/v0.5/cloudflare-kv-blind-hash");
    let rv_secret_seed = hkdf_expand_32(&prk, b"luke-share/v0.5/iroh-rendezvous-seed");

    Ok(DerivedShortCodeKeys {
        aes_key,
        kv_blind_hash: hex::encode(&kv_raw[..16]),
        rv_secret_seed,
    })
}

/// Criptografa o `EndpointAddr` com compressão Deflate + AES-256-GCM autenticado (HKDF).
pub fn encrypt_endpoint_addr(code: &str, addr: &EndpointAddr) -> Result<Vec<u8>> {
    let keys = derive_short_code_keys(code)?;
    let cipher = Aes256Gcm::new_from_slice(&keys.aes_key)
        .map_err(|e| anyhow!("Falha ao inicializar cifra AES-256-GCM: {e}"))?;

    let json = serde_json::to_vec(addr)?;
    let compressed = miniz_oxide::deflate::compress_to_vec(&json, 6);

    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &compressed,
                aad: LUKE_SHORTCODE_AAD,
            },
        )
        .map_err(|e| anyhow!("Falha na criptografia AES-256-GCM: {e}"))?;

    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Descriptografa e valida autenticidade AES-256-GCM do `EndpointAddr`.
pub fn decrypt_endpoint_addr(code: &str, encrypted_blob: &[u8]) -> Result<EndpointAddr> {
    if encrypted_blob.len() < 12 + 16 {
        anyhow::bail!("Payload criptografado truncado ou inválido");
    }
    let keys = derive_short_code_keys(code)?;
    let cipher = Aes256Gcm::new_from_slice(&keys.aes_key)
        .map_err(|e| anyhow!("Falha ao inicializar cifra AES-256-GCM: {e}"))?;

    let (nonce_bytes, ciphertext) = encrypted_blob.split_at(12);
    let nonce_arr: [u8; 12] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow!("Nonce inválido"))?;
    let nonce = Nonce::from(nonce_arr);

    let compressed = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad: LUKE_SHORTCODE_AAD,
            },
        )
        .map_err(|_| anyhow!("Código incorreto ou dados adulterados (falha de autenticação AES-GCM)"))?;

    let json = miniz_oxide::inflate::decompress_to_vec(&compressed)
        .map_err(|e| anyhow!("Falha ao descomprimir ticket: {e:?}"))?;

    let addr: EndpointAddr = serde_json::from_slice(&json)?;
    Ok(addr)
}

/// Mantém o código curto ativo enquanto a transmissão durar.
pub struct ShortCodePublisher {
    pub code: String,
    stop: Arc<AtomicBool>,
    rv_endpoint: Arc<parking_lot::Mutex<Option<Endpoint>>>,
}

impl ShortCodePublisher {
    /// Inicia a publicação em segundo plano (Rendezvous P2P Iroh + Cloudflare Worker KV opcional).
    pub fn start(addr: EndpointAddr) -> Result<Self> {
        let code = generate_short_code();
        let keys = derive_short_code_keys(&code)?;
        let encrypted_blob = Arc::new(encrypt_endpoint_addr(&code, &addr)?);
        let stop = Arc::new(AtomicBool::new(false));
        let rv_endpoint: Arc<parking_lot::Mutex<Option<Endpoint>>> = Arc::new(parking_lot::Mutex::new(None));

        // 1. Publicação opcional no Cloudflare Worker KV (TTL automático de 2h = 7200s)
        if let Some(worker_url) = configured_worker_url() {
            let blob_cf = encrypted_blob.clone();
            let hash_cf = keys.kv_blind_hash.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(4)).build() {
                    let url = format!("{worker_url}/v1/room/{hash_cf}");
                    let b58 = bs58::encode(blob_cf.as_slice()).into_string();
                    let _ = client.put(&url).body(b58).send().await;
                }
            });
        }

        // 2. Rendezvous P2P Nativo Iroh (Pkarr DNS / Relay) servindo apenas o ciphertext AES-256-GCM
        let st = stop.clone();
        let ep_slot = rv_endpoint.clone();
        let seed = keys.rv_secret_seed;
        let blob_rv = encrypted_blob.clone();
        let code_log = code.clone();

        tauri::async_runtime::spawn(async move {
            let secret = SecretKey::from_bytes(&seed);
            let ep = match Endpoint::builder(presets::N0)
                .secret_key(secret)
                .alpns(vec![LUKE_RV_ALPN.to_vec()])
                .bind()
                .await
            {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("short_code: falha ao criar endpoint rendezvous P2P: {e:#}");
                    return;
                }
            };

            *ep_slot.lock() = Some(ep.clone());
            let _ = tokio::time::timeout(Duration::from_secs(3), ep.online()).await;
            tracing::info!("short_code: código blindado {code_log} publicado na rede P2P!");

            while !st.load(Ordering::SeqCst) {
                tokio::select! {
                    incoming = ep.accept() => {
                        let Some(incoming) = incoming else { break };
                        let payload = blob_rv.clone();
                        tokio::spawn(async move {
                            if let Ok(conn) = incoming.await {
                                if let Ok((mut send, _recv)) = conn.accept_bi().await {
                                    let len = (payload.len() as u32).to_be_bytes();
                                    let _ = send.write_all(&len).await;
                                    let _ = send.write_all(&payload).await;
                                    let _ = send.finish();
                                    // Mantém a conexão aberta brevemente até o par receber os bytes
                                    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                                }
                            }
                        });
                    }
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                }
            }
            let _ = ep.close().await;
        });

        Ok(Self {
            code,
            stop,
            rv_endpoint,
        })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(ep) = self.rv_endpoint.lock().take() {
            tauri::async_runtime::spawn(async move {
                let _ = ep.close().await;
            });
        }
    }
}

impl Drop for ShortCodePublisher {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn resolve_via_worker(worker_url: &str, kv_blind_hash: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(2800))
        .build()?;
    let url = format!("{worker_url}/v1/room/{kv_blind_hash}");
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Código não encontrado no Worker ({})", resp.status());
    }
    let text = resp.text().await?;
    let bytes = bs58::decode(text.trim())
        .into_vec()
        .context("Ciphertext Base58 inválido no Worker")?;
    Ok(bytes)
}

async fn resolve_via_iroh_rv(rv_secret_seed: &[u8; 32], hint_addr: Option<EndpointAddr>) -> Result<Vec<u8>> {
    let target_id = SecretKey::from_bytes(rv_secret_seed).public();
    let target: EndpointAddr = hint_addr.unwrap_or_else(|| target_id.into());

    let client_ep = Endpoint::builder(presets::N0).bind().await?;
    let res = async {
        let conn = tokio::time::timeout(
            Duration::from_secs(8),
            client_ep.connect(target, LUKE_RV_ALPN),
        )
        .await
        .map_err(|_| anyhow!("Tempo esgotado buscando código curto na rede P2P"))??;

        let (mut send, mut recv) = conn.open_bi().await?;
        let _ = send.finish();

        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > 32 * 1024 {
            anyhow::bail!("Resposta de rendezvous com tamanho inválido ({len})");
        }
        let mut blob = vec![0u8; len];
        recv.read_exact(&mut blob).await?;
        conn.close(0u32.into(), b"ok");
        Ok::<Vec<u8>, anyhow::Error>(blob)
    }
    .await;

    let _ = client_ep.close().await;
    res
}

/// Resolve um código curto `LK-XXXX-XXXX` para o `RoomTicket` real descriptografado localmente.
pub async fn resolve_short_code(code: &str) -> Result<RoomTicket> {
    let keys = derive_short_code_keys(code)?;

    let encrypted_blob = if let Some(worker_url) = configured_worker_url() {
        let hash = keys.kv_blind_hash.clone();
        let seed = keys.rv_secret_seed;
        tokio::select! {
            res_cf = resolve_via_worker(&worker_url, &hash) => {
                match res_cf {
                    Ok(blob) => blob,
                    Err(_) => resolve_via_iroh_rv(&seed, None).await?,
                }
            }
            res_rv = resolve_via_iroh_rv(&seed, None) => {
                match res_rv {
                    Ok(blob) => blob,
                    Err(_) => resolve_via_worker(&worker_url, &hash).await?,
                }
            }
        }
    } else {
        resolve_via_iroh_rv(&keys.rv_secret_seed, None).await?
    };

    let addr = decrypt_endpoint_addr(code, &encrypted_blob)?;
    RoomTicket::new(&addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::p2p::create_endpoint;

    #[test]
    fn short_code_generation_and_normalization() {
        for _ in 0..50 {
            let code = generate_short_code();
            assert_eq!(code.len(), 12);
            assert!(code.starts_with("LK-"));
            assert!(!code.contains('0') && !code.contains('O') && !code.contains('1') && !code.contains('I'));
            assert!(is_short_code(&code));
            assert!(is_short_code(&code.to_lowercase()));
            assert!(is_short_code(&code[3..])); // sem prefixo LK-
        }
        assert!(!is_short_code("lk-eyJpZCI6IjEyMzQ1Njc4OTAiLCJhZGRycyI6W119"));
        assert!(!is_short_code("192.168.0.1"));
    }

    #[tokio::test]
    async fn short_code_aes_gcm_zero_knowledge_roundtrip() -> Result<()> {
        let ep = create_endpoint().await?;
        let addr = ep.addr();
        let code = generate_short_code();

        let blob = encrypt_endpoint_addr(&code, &addr)?;
        let recovered = decrypt_endpoint_addr(&code, &blob)?;
        assert_eq!(addr.id, recovered.id);

        // Código errado jamais consegue descriptografar o blob AES-256-GCM
        let wrong_code = "LK-ZZZZ-ZZZZ";
        if wrong_code != code {
            assert!(decrypt_endpoint_addr(wrong_code, &blob).is_err());
        }

        // Adulteração de 1 bit no ciphertext falha na tag GCM
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(decrypt_endpoint_addr(&code, &tampered).is_err());

        let _ = ep.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn short_code_iroh_rendezvous_e2e() -> Result<()> {
        let host_ep = create_endpoint().await?;
        let host_addr = host_ep.addr();
        let code = generate_short_code();
        let keys = derive_short_code_keys(&code)?;
        let blob = Arc::new(encrypt_endpoint_addr(&code, &host_addr)?);

        let rv_ep = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&keys.rv_secret_seed))
            .alpns(vec![LUKE_RV_ALPN.to_vec()])
            .bind()
            .await?;
        let rv_addr = rv_ep.addr();

        let payload = blob.clone();
        let srv = tokio::spawn(async move {
            let inc = rv_ep.accept().await.unwrap();
            let conn = inc.await.unwrap();
            let (mut send, _recv) = conn.accept_bi().await.unwrap();
            send.write_all(&(payload.len() as u32).to_be_bytes()).await.unwrap();
            send.write_all(&payload).await.unwrap();
            send.finish().unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(1), conn.closed()).await;
            let _ = rv_ep.close().await;
        });

        let fetched = resolve_via_iroh_rv(&keys.rv_secret_seed, Some(rv_addr)).await?;
        let recovered = decrypt_endpoint_addr(&code, &fetched)?;
        assert_eq!(recovered.id, host_addr.id);

        let _ = srv.await;
        let _ = host_ep.close().await;
        Ok(())
    }
}
