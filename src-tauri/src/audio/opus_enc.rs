//! Codificador Opus 48 kHz estéreo, quadros de 20 ms.

use anyhow::Result;
use opus::{Application, Bitrate, Channels, Decoder, Encoder};

pub struct OpusEnc {
    enc: Encoder,
    buf: Vec<u8>,
}

impl OpusEnc {
    pub fn new(bitrate_bps: i32) -> Result<Self> {
        let mut enc = Encoder::new(48_000, Channels::Stereo, Application::Audio)?;
        enc.set_bitrate(Bitrate::Bits(bitrate_bps))?;
        Ok(Self { enc, buf: vec![0u8; 1500] })
    }

    /// `pcm`: 960 amostras estéreo intercaladas (1920 f32).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<&[u8]> {
        let n = self.enc.encode_float(pcm, &mut self.buf)?;
        Ok(&self.buf[..n])
    }
}

pub struct OpusDec(Decoder);

impl OpusDec {
    pub fn new() -> Result<Self> {
        Ok(Self(Decoder::new(48_000, Channels::Stereo)?))
    }
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>> {
        let mut out = vec![0f32; 960 * 2];
        let n = self.0.decode_float(packet, &mut out, false)?;
        out.truncate(n * 2);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::analysis::*;

    #[test]
    fn opus_roundtrip_preserves_tone() {
        let mut e = OpusEnc::new(128_000).unwrap();
        let mut d = OpusDec::new().unwrap();
        let mut decoded = Vec::new();
        for f in 0..50 {
            let pcm: Vec<f32> = (0..960)
                .flat_map(|i| {
                    let t = (f * 960 + i) as f32 / 48000.0;
                    let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                    [v, v]
                })
                .collect();
            let pkt = e.encode(&pcm).unwrap().to_vec();
            assert!(pkt.len() < 1200, "pacote Opus precisa caber no MTU");
            decoded.extend(d.decode(&pkt).unwrap());
        }
        let mono = to_mono(&decoded[1920 * 5..], 2);
        assert!(goertzel_amp(&mono, 48000.0, 440.0) > 0.4);
    }
}
