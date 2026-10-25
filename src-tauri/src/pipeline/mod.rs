//! Pipeline de transmissão e recepção de mídia:
//! - Vídeo via IPC Binário direto (`InvokeResponseBody::Raw` → `ArrayBuffer` zero-copy no WebCodecs, sem Base64).
//! - Áudio reproduzido nativamente via WASAPI no Rust (`audio::player::NativeAudioPlayer`), imune a throttling em background.

pub mod receiver;
pub mod sender;

use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};

pub const BIN_VIDEO_MAGIC: u8 = 0x56; // 'V'
pub const BIN_VIDEO_FLAG_KEY: u8 = 0x01;
pub const BIN_VIDEO_HEADER_LEN: usize = 8;

/// Empacota um quadro de vídeo codificado (Annex-B H.264/HEVC/AV1) com cabeçalho binário de 8 bytes
/// para entrega direta via `ArrayBuffer` ao WebView2 sem codificação Base64/JSON:
/// `[0x56, flags, 0, 0, pts_u32_le(4B), ...bitstream]`
pub fn pack_binary_video_ipc(key: bool, pts: u32, bitstream: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(BIN_VIDEO_HEADER_LEN + bitstream.len());
    out.push(BIN_VIDEO_MAGIC);
    out.push(if key { BIN_VIDEO_FLAG_KEY } else { 0 });
    out.push(0);
    out.push(0);
    out.extend_from_slice(&pts.to_le_bytes());
    out.extend_from_slice(bitstream);
    out
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t")]
pub enum StreamEvent {
    /// Quadro binário despachado diretamente como `InvokeResponseBody::Raw` (ArrayBuffer no JS).
    #[serde(skip)]
    VideoBin {
        key: bool,
        pts: u32,
        data: Vec<u8>,
    },
    #[serde(rename = "config")]
    Config {
        width: u32,
        height: u32,
        fps: u32,
        #[serde(default = "default_codec_event")]
        codec: String,
    },
    #[serde(rename = "audio_state")]
    AudioState {
        enabled: bool,
    },
    #[serde(rename = "viewers")]
    Viewers {
        count: u32,
        viewers: Vec<String>,
    },
    #[serde(rename = "reconnecting")]
    Reconnecting {
        attempt: u32,
        remaining_secs: u32,
    },
    #[serde(rename = "reconnected")]
    Reconnected,
    #[serde(rename = "ended")]
    Ended {
        reason: String,
    },
    #[serde(rename = "telemetry")]
    Telemetry {
        rtt_ms: u32,
        packets_rx: u64,
        loss_pct: f32,
        nacks_recovered: u64,
        #[serde(default)]
        fec_recovered: u64,
        dropped_ref: u64,
        fps: u32,
        #[serde(default)]
        via_relay: bool,
    },
}

fn default_codec_event() -> String {
    "h264".into()
}

/// Envia `StreamEvent` pelo canal IPC do Tauri v2:
/// - `VideoBin` vai como `InvokeResponseBody::Raw` (`ArrayBuffer` direto sem Base64).
/// - Eventos de controle/telemetria vão como JSON leve (`InvokeResponseBody::Json`).
pub fn dispatch_ipc_event(chan: &Channel<InvokeResponseBody>, ev: StreamEvent) {
    match ev {
        StreamEvent::VideoBin { key, pts, data } => {
            let raw = pack_binary_video_ipc(key, pts, &data);
            let _ = chan.send(InvokeResponseBody::Raw(raw));
        }
        other => {
            if let Ok(json) = serde_json::to_string(&other) {
                let _ = chan.send(InvokeResponseBody::Json(json));
            }
        }
    }
}

/// Codificação Base64 utilitária (usada apenas na serialização compacta do `RoomTicket` `lk-...`).
pub fn base64_encode(data: &[u8]) -> String {
    const CHARSET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };

        out.push(CHARSET[(b0 >> 2) as usize] as char);
        out.push(CHARSET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARSET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARSET[(b2 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_video_ipc_framing() {
        let payload = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42];
        let framed = pack_binary_video_ipc(true, 180_000, &payload);
        assert_eq!(framed.len(), BIN_VIDEO_HEADER_LEN + payload.len());
        assert_eq!(framed[0], BIN_VIDEO_MAGIC);
        assert_eq!(framed[1], BIN_VIDEO_FLAG_KEY);
        assert_eq!(u32::from_le_bytes(framed[4..8].try_into().unwrap()), 180_000);
        assert_eq!(&framed[8..], &payload[..]);
    }
}
