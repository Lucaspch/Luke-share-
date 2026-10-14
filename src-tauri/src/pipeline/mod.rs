//! Pipeline de transmissão e recepção de mídia (vídeo H.264 + áudio Opus/PCM).

pub mod receiver;
pub mod sender;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t")]
pub enum StreamEvent {
    #[serde(rename = "video")]
    Video {
        #[serde(rename = "k")]
        key: bool,
        #[serde(rename = "p")]
        pts: u32,
        #[serde(rename = "d")]
        data: String,
    },
    #[serde(rename = "audio")]
    Audio {
        #[serde(rename = "d")]
        data: String,
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
    },
}

fn default_codec_event() -> String {
    "h264".into()
}

/// Codificação Base64 eficiente sem alocações extras além da String de saída.
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
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
