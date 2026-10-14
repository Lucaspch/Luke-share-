//! Presets de resolução: Origem / 1080p / 720p / 480p, preservando o aspect ratio.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Source,
    P1080,
    P720,
    P480,
}

impl Preset {
    /// Caixa máxima do preset (None = nativa).
    pub fn bounding_box(self) -> Option<(u32, u32)> {
        match self {
            Preset::Source => None,
            Preset::P1080 => Some((1920, 1080)),
            Preset::P720 => Some((1280, 720)),
            Preset::P480 => Some((854, 480)),
        }
    }

    /// Bitrate alvo padrão (bps) para 1 espectador.
    pub fn default_bitrate(self) -> u32 {
        match self {
            Preset::Source => 6_000_000,
            Preset::P1080 => 4_500_000,
            Preset::P720 => 2_500_000,
            Preset::P480 => 1_200_000,
        }
    }

    /// Bitrate adaptativo por espectador baseado no número de pessoas assistindo.
    pub fn adaptive_bitrate(self, viewers_count: usize) -> u32 {
        let base = self.default_bitrate();
        match viewers_count {
            0 | 1 => base,
            2 => (base * 4 / 5).max(2_000_000),
            3 => (base * 3 / 5).max(1_500_000),
            _ => (base / 2).max(1_200_000),
        }
    }
}

fn even_round(v: f64) -> u32 {
    let n = (v / 2.0).round() as u32 * 2;
    n.max(2)
}

/// Tamanho de saída: cabe na caixa do preset, mantém o aspect ratio, é par (H.264 4:2:0)
/// e **nunca amplia** a fonte.
pub fn target_size(src_w: u32, src_h: u32, p: Preset) -> (u32, u32) {
    let (src_w, src_h) = (src_w.max(2), src_h.max(2));
    let Some((bw, bh)) = p.bounding_box() else { return (src_w & !1, src_h & !1) };
    let scale = (bw as f64 / src_w as f64).min(bh as f64 / src_h as f64).min(1.0);
    (even_round(src_w as f64 * scale), even_round(src_h as f64 * scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_cases() {
        assert_eq!(target_size(2560, 1440, Preset::P1080), (1920, 1080));
        assert_eq!(target_size(3440, 1440, Preset::P1080), (1920, 804));
        assert_eq!(target_size(1000, 700, Preset::P720), (1000, 700)); // sem upscale
        assert_eq!(target_size(1920, 1080, Preset::P480), (854, 480));
        assert_eq!(target_size(1080, 1920, Preset::P720), (406, 720)); // retrato
        assert_eq!(target_size(1920, 1080, Preset::Source), (1920, 1080));
        assert_eq!(target_size(1921, 1081, Preset::Source), (1920, 1080)); // par
        assert_eq!(target_size(3840, 2160, Preset::P720), (1280, 720));
    }

    #[test]
    fn properties_even_inside_box_aspect_preserved() {
        for &(w, h) in &[(1920u32, 1080u32), (2560, 1080), (800, 600), (1366, 768), (5120, 1440), (721, 1281), (3840, 2160), (640, 480)] {
            for p in [Preset::P1080, Preset::P720, Preset::P480] {
                let (ow, oh) = target_size(w, h, p);
                let (bw, bh) = p.bounding_box().unwrap();
                assert!(ow % 2 == 0 && oh % 2 == 0, "{w}x{h} {p:?} -> {ow}x{oh}");
                assert!(ow <= bw.max(w.min(bw)) + 1 && oh <= bh + 1 || (ow <= w && oh <= h));
                assert!(ow <= w && oh <= h, "não amplia");
                let err = ((ow as f64 / oh as f64) - (w as f64 / h as f64)).abs() * oh as f64;
                assert!(err <= 1.5, "aspect {w}x{h} {p:?} -> {ow}x{oh} err {err}");
            }
        }
    }
}
