//! Análise de sinal para testes de áudio (Goertzel) e escrita de WAV.

use std::io::Write;

/// Potência (RMS) normalizada do tom de `freq` Hz em `samples` (mono), em amplitude ≈ pico do tom.
pub fn goertzel_amp(samples: &[f32], sample_rate: f32, freq: f32) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let w = 2.0 * std::f32::consts::PI * freq / sample_rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / n as f32
}

pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    interleaved.chunks_exact(channels).map(|c| c.iter().sum::<f32>() / channels as f32).collect()
}

pub fn db(a: f32) -> f32 {
    20.0 * a.max(1e-9).log10()
}

/// WAV float32.
pub fn write_wav_f32(path: &std::path::Path, samples: &[f32], channels: u16, rate: u32) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let data_len = (samples.len() * 4) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?;
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * channels as u32 * 4).to_le_bytes())?;
    f.write_all(&(channels * 4).to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48000.0).sin()).collect()
    }

    #[test]
    fn goertzel_detects_only_present_tone() {
        let mut s = sine(440.0, 0.5, 48000);
        for (a, b) in s.iter_mut().zip(sine(880.0, 0.0, 48000)) {
            *a += b;
        }
        let a440 = goertzel_amp(&s, 48000.0, 440.0);
        let a880 = goertzel_amp(&s, 48000.0, 880.0);
        assert!((a440 - 0.5).abs() < 0.02, "{a440}");
        assert!(db(a440) - db(a880) > 40.0);
    }
}
