//! Utilidades H.264 Annex B (puro, sem Win32).

/// Itera sobre as NAL units de um stream Annex B (sem os start codes).
pub fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (posição do start code, tamanho do start code)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, 3));
                i += 3;
                continue;
            } else if data[i + 2] == 0 && i + 4 <= data.len() && data[i + 3] == 1 {
                starts.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    let mut out = Vec::new();
    for (k, (pos, len)) in starts.iter().enumerate() {
        let s = pos + len;
        let e = starts.get(k + 1).map(|(p, _)| *p).unwrap_or(data.len());
        if s < e {
            out.push(&data[s..e]);
        }
    }
    out
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

pub const NAL_HEVC_VPS: u8 = 32;
pub const NAL_HEVC_SPS: u8 = 33;
pub const NAL_HEVC_PPS: u8 = 34;
pub const NAL_HEVC_IDR_W_RADL: u8 = 19;
pub const NAL_HEVC_IDR_N_LP: u8 = 20;
pub const NAL_HEVC_CRA: u8 = 21;

pub fn hevc_nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0)
}

pub fn has_nal(data: &[u8], t: u8) -> bool {
    nals(data).iter().any(|n| nal_type(n) == t)
}

/// Guarda VPS/SPS/PPS vistos e garante que todo keyframe os carregue (decoders novos precisam deles).
#[derive(Default)]
pub struct ParamSets {
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    is_hevc: bool,
}

impl ParamSets {
    pub fn new(is_hevc: bool) -> Self {
        Self {
            vps: None,
            sps: None,
            pps: None,
            is_hevc,
        }
    }

    pub fn set_from_annexb(&mut self, data: &[u8]) {
        for n in nals(data) {
            if self.is_hevc {
                match hevc_nal_type(n) {
                    NAL_HEVC_VPS => self.vps = Some(n.to_vec()),
                    NAL_HEVC_SPS => self.sps = Some(n.to_vec()),
                    NAL_HEVC_PPS => self.pps = Some(n.to_vec()),
                    _ => {}
                }
            } else {
                match nal_type(n) {
                    NAL_SPS => self.sps = Some(n.to_vec()),
                    NAL_PPS => self.pps = Some(n.to_vec()),
                    _ => {}
                }
            }
        }
    }

    /// Retorna o frame pronto para envio; em keyframes sem SPS/PPS, prefixa os guardados.
    pub fn ensure_in_keyframe(&mut self, frame: Vec<u8>) -> Vec<u8> {
        self.set_from_annexb(&frame);
        if self.is_hevc {
            let is_idr = nals(&frame).iter().any(|n| {
                let t = hevc_nal_type(n);
                t == NAL_HEVC_IDR_W_RADL || t == NAL_HEVC_IDR_N_LP || t == NAL_HEVC_CRA
            });
            let has_headers = nals(&frame).iter().any(|n| hevc_nal_type(n) == NAL_HEVC_VPS)
                && nals(&frame).iter().any(|n| hevc_nal_type(n) == NAL_HEVC_SPS)
                && nals(&frame).iter().any(|n| hevc_nal_type(n) == NAL_HEVC_PPS);

            if is_idr && !has_headers {
                if let (Some(v), Some(s), Some(p)) = (&self.vps, &self.sps, &self.pps) {
                    let mut out = Vec::with_capacity(frame.len() + v.len() + s.len() + p.len() + 12);
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(v);
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(s);
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(p);
                    out.extend_from_slice(&frame);
                    return out;
                }
            }
        } else {
            let is_idr = has_nal(&frame, NAL_IDR);
            if is_idr && !(has_nal(&frame, NAL_SPS) && has_nal(&frame, NAL_PPS)) {
                if let (Some(s), Some(p)) = (&self.sps, &self.pps) {
                    let mut out = Vec::with_capacity(frame.len() + s.len() + p.len() + 8);
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(s);
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(p);
                    out.extend_from_slice(&frame);
                    return out;
                }
            }
        }
        frame
    }
}


// ---------------------------------------------------------------------------------------------
// Parser mínimo de SPS (largura/altura) — usado em testes e para validar o encoder.
// ---------------------------------------------------------------------------------------------

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i32;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Retorna (largura, altura, profile_idc, level_idc) de uma NAL SPS (com o byte de cabeçalho).
pub fn parse_sps(nal: &[u8]) -> Option<(u32, u32, u8, u8)> {
    if nal_type(nal) != NAL_SPS {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut b = Bits { d: &rbsp, pos: 0 };
    let profile = b.bits(8)? as u8;
    b.bits(8)?;
    let level = b.bits(8)? as u8;
    b.ue()?;
    let mut chroma = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = b.ue()?;
        if chroma == 3 {
            b.bit()?;
        }
        b.ue()?;
        b.ue()?;
        b.bit()?;
        if b.bit()? == 1 {
            for i in 0..(if chroma != 3 { 8 } else { 12 }) {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    b.ue()?;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?;
    b.bit()?;
    let w_mbs = b.ue()? + 1;
    let h_map = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?;
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if b.bit()? == 1 {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let (cux, cuy) = if chroma == 0 { (1, 2 - frame_mbs_only) } else { (2, 2 * (2 - frame_mbs_only)) };
    let w = w_mbs * 16 - cux * (cl + cr);
    let h = (2 - frame_mbs_only) * h_map * 16 - cuy * (ct + cb);
    Some((w, h, profile, level))
}

/// Dimensões do primeiro SPS encontrado em um stream Annex B.
pub fn stream_size(data: &[u8]) -> Option<(u32, u32)> {
    nals(data).into_iter().find_map(|n| parse_sps(n).map(|(w, h, _, _)| (w, h)))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn nal(t: u8, n: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, t];
        v.extend(std::iter::repeat(0xAB).take(n));
        v
    }

    #[test]
    fn sps_1080p_high_profile() {
        let sps = [
            0x67u8, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03, 0x00,
            0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
        ];
        let (w, h, profile, level) = parse_sps(&sps).unwrap();
        assert_eq!((w, h, profile, level), (1920, 1080, 100, 40));
        let mut annexb = vec![0, 0, 0, 1];
        annexb.extend_from_slice(&sps);
        assert_eq!(stream_size(&annexb), Some((1920, 1080)));
    }

    #[test]
    fn parses_mixed_start_codes() {
        let mut d = nal(0x67, 5);
        d.extend_from_slice(&[0, 0, 1, 0x68, 1, 2]);
        d.extend(nal(0x65, 10));
        let ns = nals(&d);
        assert_eq!(ns.iter().map(|n| nal_type(n)).collect::<Vec<_>>(), vec![7, 8, 5]);
        assert!(has_nal(&d, NAL_IDR));
    }

    #[test]
    fn keyframe_gets_param_sets_prepended() {
        let mut ps = ParamSets::default();
        let first = [nal(0x67, 4), nal(0x68, 2), nal(0x65, 20)].concat();
        let out = ps.ensure_in_keyframe(first.clone());
        assert_eq!(out, first); // já tinha tudo
        let idr_only = nal(0x65, 30);
        let out = ps.ensure_in_keyframe(idr_only.clone());
        assert!(has_nal(&out, NAL_SPS) && has_nal(&out, NAL_PPS) && has_nal(&out, NAL_IDR));
        assert_eq!(&out[out.len() - idr_only.len()..], &idr_only[..]);
        // P-frame não é alterado
        let p = nal(0x41, 30);
        assert_eq!(ps.ensure_in_keyframe(p.clone()), p);
    }
}
