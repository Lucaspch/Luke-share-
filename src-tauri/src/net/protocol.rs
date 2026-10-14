//! Protocolo: mensagens de controle (TCP, JSON com prefixo de tamanho) e cabeçalho de mídia (UDP).

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

pub const DISCOVERY_PORT: u16 = 47800;
pub const CONTROL_PORT: u16 = 47801;
pub const MAGIC: &str = "LKSH";
pub const PROTO_VERSION: u8 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeerStatus {
    Idle,
    Streaming,
    Watching,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VideoOffer {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AudioOffer {
    pub sample_rate: u32,
    pub channels: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum Control {
    /// viewer → sharer
    Watch { peer_id: Uuid, nick: String, udp_port: u16 },
    /// sharer → viewer
    Accept { video: VideoOffer, audio: Option<AudioOffer>, udp_port: u16 },
    Reject { reason: String },
    /// sharer → viewer: dimensões mudaram
    Reconfigure { video: VideoOffer },
    /// sharer → viewer: áudio ligado/desligado
    AudioState { enabled: bool },
    Bye,
}

pub async fn write_control<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &Control) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

pub async fn read_control<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Control> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    if n > 64 * 1024 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "mensagem de controle grande demais"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ---------------------------------------------------------------------------------------------
// Cabeçalho de mídia (16 bytes, big-endian)
// ---------------------------------------------------------------------------------------------

pub const MEDIA_MAGIC: u8 = 0xA7;
pub const HEADER_LEN: usize = 16;
pub const MAX_PAYLOAD: usize = 1100; // 1100 + 16 HEADER_LEN = 1116 bytes (garante compatibilidade estrita com o limite de 1162 bytes do QUIC Datagram Iroh e túneis VPN)

pub const KIND_VIDEO: u8 = 1;
pub const KIND_AUDIO: u8 = 2;
pub const KIND_NACK: u8 = 3;
pub const KIND_PLI: u8 = 4;
pub const KIND_HELLO: u8 = 5; // viewer → sharer: abre o "furo" UDP e informa o endpoint de origem

pub const FLAG_KEY: u8 = 0b01;
pub const FLAG_LAST: u8 = 0b10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaHeader {
    pub kind: u8,
    pub flags: u8,
    pub frame_id: u32,
    pub frag_idx: u16,
    pub frag_cnt: u16,
    pub ts_90k: u32,
}

impl MediaHeader {
    pub fn is_key(&self) -> bool {
        self.flags & FLAG_KEY != 0
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(MEDIA_MAGIC);
        out.push(self.kind);
        out.push(self.flags);
        out.push(0);
        out.extend_from_slice(&self.frame_id.to_be_bytes());
        out.extend_from_slice(&self.frag_idx.to_be_bytes());
        out.extend_from_slice(&self.frag_cnt.to_be_bytes());
        out.extend_from_slice(&self.ts_90k.to_be_bytes());
    }

    pub fn parse(buf: &[u8]) -> Option<(MediaHeader, &[u8])> {
        if buf.len() < HEADER_LEN || buf[0] != MEDIA_MAGIC {
            return None;
        }
        let h = MediaHeader {
            kind: buf[1],
            flags: buf[2],
            frame_id: u32::from_be_bytes(buf[4..8].try_into().ok()?),
            frag_idx: u16::from_be_bytes(buf[8..10].try_into().ok()?),
            frag_cnt: u16::from_be_bytes(buf[10..12].try_into().ok()?),
            ts_90k: u32::from_be_bytes(buf[12..16].try_into().ok()?),
        };
        Some((h, &buf[HEADER_LEN..]))
    }
}

pub fn control_packet(kind: u8, frame_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
    MediaHeader { kind, flags: 0, frame_id, frag_idx: 0, frag_cnt: 1, ts_90k: 0 }.write(&mut v);
    v.extend_from_slice(payload);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = MediaHeader { kind: KIND_VIDEO, flags: FLAG_KEY | FLAG_LAST, frame_id: 0xDEADBEEF, frag_idx: 7, frag_cnt: 9, ts_90k: 123456 };
        let mut v = Vec::new();
        h.write(&mut v);
        v.extend_from_slice(b"xyz");
        assert_eq!(v.len(), HEADER_LEN + 3);
        let (p, rest) = MediaHeader::parse(&v).unwrap();
        assert_eq!(p, h);
        assert_eq!(rest, b"xyz");
        assert!(MediaHeader::parse(&v[..10]).is_none());
        v[0] = 0;
        assert!(MediaHeader::parse(&v).is_none());
    }

    #[tokio::test]
    async fn control_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let m = Control::Accept { video: VideoOffer { width: 1920, height: 1080, fps: 60 }, audio: Some(AudioOffer { sample_rate: 48000, channels: 2 }), udp_port: 5000 };
        write_control(&mut a, &m).await.unwrap();
        assert_eq!(read_control(&mut b).await.unwrap(), m);
    }
}
