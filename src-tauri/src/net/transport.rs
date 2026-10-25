//! Transporte de mídia sobre UDP: fragmentação, remontagem com NACK / pedido de keyframe e
//! ring buffer de retransmissão. Toda a lógica de tempo recebe `now_ms` (testável sem relógio).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::protocol::*;

/// Fragmenta um frame/pacote em datagramas ≤ `MAX_PAYLOAD` + cabeçalho.
pub fn packetize(kind: u8, frame_id: u32, key: bool, ts_90k: u32, data: &[u8]) -> Vec<Vec<u8>> {
    let cnt = data.len().div_ceil(MAX_PAYLOAD).max(1);
    assert!(cnt <= u16::MAX as usize, "frame grande demais");
    (0..cnt)
        .map(|i| {
            let chunk = &data[(i * MAX_PAYLOAD).min(data.len())..((i + 1) * MAX_PAYLOAD).min(data.len())];
            let mut flags = if key { FLAG_KEY } else { 0 };
            if i == cnt - 1 {
                flags |= FLAG_LAST;
            }
            let mut v = Vec::with_capacity(HEADER_LEN + chunk.len());
            MediaHeader { kind, flags, frame_id, frag_idx: i as u16, frag_cnt: cnt as u16, ts_90k }.write(&mut v);
            v.extend_from_slice(chunk);
            v
        })
        .collect()
}

pub const DEFAULT_FEC_BLOCK_SIZE: usize = 8;
pub const KEYFRAME_FEC_BLOCK_SIZE: usize = 4;
pub const FEC_HEADER_BYTES: usize = 8; // start_idx (2B) + count (2B) + total_video_frags (2B) + last_frag_len (2B)

/// Gera pacotes FEC de paridade XOR para os fragmentos de vídeo de um frame.
/// Divide os fragmentos em blocos de até `block_size` fragmentos e calcula a paridade bit a bit.
pub fn generate_fec(
    frame_id: u32,
    key: bool,
    ts_90k: u32,
    packets: &[Vec<u8>],
    block_size: usize,
) -> Vec<Vec<u8>> {
    if packets.is_empty() {
        return Vec::new();
    }
    let total_frags = packets.len();
    let num_blocks = total_frags.div_ceil(block_size.max(1));
    let mut fec_pkts = Vec::with_capacity(num_blocks);

    for block_idx in 0..num_blocks {
        let start = block_idx * block_size;
        let end = (start + block_size).min(total_frags);
        let count = (end - start) as u16;

        let block_slices: Vec<&[u8]> = (start..end)
            .map(|i| {
                if packets[i].len() > HEADER_LEN {
                    &packets[i][HEADER_LEN..]
                } else {
                    &[]
                }
            })
            .collect();

        let max_len = block_slices.iter().map(|s| s.len()).max().unwrap_or(0);
        if max_len == 0 {
            continue;
        }

        let mut parity = vec![0u8; max_len];
        for slice in &block_slices {
            for (k, &byte) in slice.iter().enumerate() {
                parity[k] ^= byte;
            }
        }

        let last_frag_len = block_slices.last().map(|s| s.len() as u16).unwrap_or(0);

        let mut out = Vec::with_capacity(HEADER_LEN + FEC_HEADER_BYTES + max_len);
        let flags = if key { FLAG_KEY } else { 0 };
        MediaHeader {
            kind: KIND_FEC,
            flags,
            frame_id,
            frag_idx: block_idx as u16,
            frag_cnt: num_blocks as u16,
            ts_90k,
        }
        .write(&mut out);

        out.extend_from_slice(&(start as u16).to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&(total_frags as u16).to_be_bytes());
        out.extend_from_slice(&last_frag_len.to_be_bytes());
        out.extend_from_slice(&parity);

        fec_pkts.push(out);
    }

    fec_pkts
}

/// Guarda os últimos N datagramas de vídeo enviados para atender NACKs.
pub struct RetransmitRing {
    cap: usize,
    q: VecDeque<(u32, u16, Arc<Vec<u8>>)>,
}

impl RetransmitRing {
    pub fn new(cap: usize) -> Self {
        Self { cap, q: VecDeque::with_capacity(cap) }
    }
    pub fn push(&mut self, frame_id: u32, frag_idx: u16, pkt: Vec<u8>) {
        if self.q.len() == self.cap {
            self.q.pop_front();
        }
        self.q.push_back((frame_id, frag_idx, Arc::new(pkt)));
    }
    pub fn get(&self, frame_id: u32, frag_idx: u16) -> Option<Arc<Vec<u8>>> {
        self.q.iter().rev().find(|(f, i, _)| *f == frame_id && *i == frag_idx).map(|(_, _, p)| p.clone())
    }
}

/// Controlador de Congestionamento Adaptativo (AIMD) para taxa de bits e nível de FEC do encoder.
/// Reduz multiplicativamente sob congestionamento (perda > 2.0% ou RTT alto)
/// e aumenta aditivamente quando o canal está estável.
#[derive(Debug, Clone)]
pub struct CongestionController {
    min_bitrate: u32,
    max_bitrate: u32,
    current_bitrate: u32,
    clean_ticks: u32,
    last_decrease: Option<Instant>,
    loss_ewma: f32,
    rtt_ewma: u32,
}

impl CongestionController {
    pub fn new(min_bitrate: u32, max_bitrate: u32, initial_bitrate: u32) -> Self {
        Self {
            min_bitrate,
            max_bitrate,
            current_bitrate: initial_bitrate.clamp(min_bitrate, max_bitrate),
            clean_ticks: 0,
            last_decrease: None,
            loss_ewma: 0.0,
            rtt_ewma: 0,
        }
    }

    pub fn current_bitrate(&self) -> u32 {
        self.current_bitrate
    }

    /// Retorna o tamanho ideal do bloco FEC (menor = proteção mais forte).
    /// Keyframes e redes com perda recente recebem paridade reforçada para evitar loops de congelamento.
    pub fn recommended_fec_block_size(&self, is_key: bool) -> usize {
        if self.loss_ewma > 4.0 {
            if is_key { 2 } else { 3 } // 50% FEC em Keyframe / 33% em P-frame sob perda severa
        } else if self.loss_ewma > 1.0 {
            if is_key { 3 } else { 4 } // 33% FEC em Keyframe / 25% em P-frame sob perda moderada
        } else if is_key {
            KEYFRAME_FEC_BLOCK_SIZE // 25% FEC em todo Keyframe para blindar entrada e retomada
        } else {
            DEFAULT_FEC_BLOCK_SIZE // 12.5% FEC em canal limpo
        }
    }

    pub fn set_max_bitrate(&mut self, max: u32) {
        self.max_bitrate = max;
        self.min_bitrate = (max / 6).clamp(1_200_000, 2_500_000).min(max);
        if self.current_bitrate > self.max_bitrate {
            self.current_bitrate = self.max_bitrate;
        } else if self.current_bitrate < self.min_bitrate {
            self.current_bitrate = self.min_bitrate;
        }
    }

    /// Processa telemetria recebida de um espectador.
    /// Retorna `Some(new_bitrate)` se a taxa de bits foi alterada.
    pub fn on_telemetry(&mut self, rtt_ms: u32, loss_pct: f32) -> Option<u32> {
        let now = Instant::now();

        self.loss_ewma = if self.loss_ewma == 0.0 {
            loss_pct
        } else {
            self.loss_ewma * 0.6 + loss_pct * 0.4
        };
        if rtt_ms > 0 {
            self.rtt_ewma = if self.rtt_ewma == 0 {
                rtt_ms
            } else {
                (self.rtt_ewma * 3 + rtt_ms) / 4
            };
        }

        // Condição de congestionamento: Perda > 2.0% ou RTT severo (> 230ms)
        if loss_pct > 2.0 || rtt_ms > 230 {
            if let Some(last) = self.last_decrease {
                if now.duration_since(last) < Duration::from_millis(850) {
                    return None;
                }
            }

            // Redução Multiplicativa (AIMD): 15%, 25% ou 35% dependendo da gravidade
            let factor = if loss_pct > 12.0 {
                0.65
            } else if loss_pct > 8.0 {
                0.75
            } else {
                0.85
            };
            let new_br = ((self.current_bitrate as f32 * factor) as u32).max(self.min_bitrate);

            self.clean_ticks = 0;
            self.last_decrease = Some(now);

            if new_br != self.current_bitrate {
                tracing::info!(
                    "congestion_control: AIMD decréscimo multiplicativo: perda={:.1}%, RTT={}ms -> {} -> {} bps",
                    loss_pct, rtt_ms, self.current_bitrate, new_br
                );
                self.current_bitrate = new_br;
                return Some(new_br);
            }
        } else if loss_pct <= 0.8 && rtt_ms <= 180 {
            // Protege contra aumento prematuro caso outro espectador tenha reportado perda recentemente
            if let Some(last) = self.last_decrease {
                if now.duration_since(last) < Duration::from_millis(2200) {
                    return None;
                }
            }
            // Canal limpo: após 2 relatórios consecutivos, aplica Aumento Aditivo
            self.clean_ticks += 1;
            if self.clean_ticks >= 2 && self.current_bitrate < self.max_bitrate {
                self.clean_ticks = 0;
                let step = (self.max_bitrate / 15).max(800_000);
                let new_br = (self.current_bitrate + step).min(self.max_bitrate);
                if new_br != self.current_bitrate {
                    tracing::info!(
                        "congestion_control: AIMD acréscimo aditivo: canal estável -> {} -> {} bps",
                        self.current_bitrate, new_br
                    );
                    self.current_bitrate = new_br;
                    return Some(new_br);
                }
            }
        } else {
            // Zona neutra (0.8% a 2.0%): mantém taxa atual para evitar oscilações
            self.clean_ticks = 0;
        }

        None
    }
}

pub fn build_nack(frame_id: u32, missing: &[u16]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(missing.len() * 2);
    for m in missing.iter().take(MAX_PAYLOAD / 2) {
        payload.extend_from_slice(&m.to_be_bytes());
    }
    control_packet(KIND_NACK, frame_id, &payload)
}

pub fn parse_nack_payload(payload: &[u8]) -> Vec<u16> {
    payload.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect()
}

#[derive(Debug, PartialEq)]
pub enum RxEvent {
    Frame { frame_id: u32, key: bool, ts_90k: u32, data: Vec<u8> },
    Nack { frame_id: u32, missing: Vec<u16> },
    RequestKey,
}

#[derive(Clone, Debug)]
pub struct FecBlock {
    pub start_idx: u16,
    pub count: u16,
    pub total_video_frags: u16,
    pub last_frag_len: u16,
    pub parity: Vec<u8>,
}

struct Partial {
    cnt: u16,
    frags: Vec<Option<Vec<u8>>>,
    got: u16,
    key: bool,
    ts: u32,
    last_nack: u64,
    nacks: u8,
    fec_blocks: Vec<FecBlock>,
}

impl Partial {
    fn complete(&self) -> bool {
        self.got == self.cnt
    }
    fn missing(&self) -> Vec<u16> {
        self.frags.iter().enumerate().filter(|(_, f)| f.is_none()).map(|(i, _)| i as u16).collect()
    }

    /// Tenta recuperar fragmentos faltantes usando os blocos FEC acumulados.
    /// Retorna o número de fragmentos reconstruídos com sucesso via XOR.
    fn try_fec_recovery(&mut self) -> usize {
        let mut recovered_count = 0;
        let mut progress = true;

        while progress {
            progress = false;
            for fec in &self.fec_blocks {
                let start = fec.start_idx as usize;
                let end = (fec.start_idx + fec.count) as usize;
                if start >= self.frags.len() || end > self.frags.len() {
                    continue;
                }

                let missing_in_block: Vec<usize> = (start..end)
                    .filter(|&i| self.frags[i].is_none())
                    .collect();

                // Se falta exatamente 1 pacote neste bloco FEC, conseguimos reconstruí-lo instantaneamente!
                if missing_in_block.len() == 1 {
                    let missing_idx = missing_in_block[0];
                    let expected_len = if missing_idx == (self.cnt as usize) - 1 {
                        fec.last_frag_len as usize
                    } else {
                        MAX_PAYLOAD.min(fec.parity.len())
                    };

                    let mut recovered = fec.parity[..expected_len.min(fec.parity.len())].to_vec();
                    for i in start..end {
                        if i != missing_idx {
                            if let Some(other_frag) = &self.frags[i] {
                                for (k, &b) in other_frag.iter().take(recovered.len()).enumerate() {
                                    recovered[k] ^= b;
                                }
                            }
                        }
                    }

                    self.frags[missing_idx] = Some(recovered);
                    self.got += 1;
                    recovered_count += 1;
                    progress = true; // Pode desbloquear a recuperação de outros blocos
                }
            }
        }

        recovered_count
    }
}

pub const NACK_AFTER_MS: u64 = 4;
pub const NACK_RETRY_MS: u64 = 15;
pub const GIVE_UP_DELTA_MS: u64 = 160;
pub const GIVE_UP_KEY_MS: u64 = 280;
pub const PLI_INTERVAL_MS: u64 = 250;

pub struct Reassembler {
    frames: BTreeMap<u32, Partial>,
    next_id: Option<u32>,
    need_key: bool,
    stall_since: Option<u64>,
    last_pli: Option<u64>,
    last_nack_empty: Option<u64>,
    pub packets_rx: u64,
    pub packets_lost: u64,
    pub nacks_sent: u64,
    pub nacks_recovered: u64,
    pub fec_recovered: u64,
    pub dropped_frames: u64,
    pub dropped_ref_frames: u64,
    pub frames_delivered: u64,
    pub rtt_ms: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReassemblerStats {
    pub packets_rx: u64,
    pub packets_lost: u64,
    pub nacks_sent: u64,
    pub nacks_recovered: u64,
    pub fec_recovered: u64,
    pub dropped_frames: u64,
    pub dropped_ref_frames: u64,
    pub frames_delivered: u64,
    pub rtt_ms: u32,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            next_id: None,
            need_key: true,
            stall_since: None,
            last_pli: None,
            last_nack_empty: None,
            packets_rx: 0,
            packets_lost: 0,
            nacks_sent: 0,
            nacks_recovered: 0,
            fec_recovered: 0,
            dropped_frames: 0,
            dropped_ref_frames: 0,
            frames_delivered: 0,
            rtt_ms: 0,
        }
    }

    pub fn waiting_for_key(&self) -> bool {
        self.need_key
    }

    /// Atualiza a estimativa de RTT a partir da conexão QUIC subjacente, permitindo
    /// ajustar prazos de NACK e Give-Up mesmo antes da primeira perda de pacote.
    pub fn set_link_rtt_ms(&mut self, quic_rtt_ms: u32) {
        if quic_rtt_ms > 0 && quic_rtt_ms < 3000 {
            if self.rtt_ms == 0 {
                self.rtt_ms = quic_rtt_ms;
            } else {
                self.rtt_ms = (self.rtt_ms * 3 + quic_rtt_ms) / 4;
            }
        }
    }

    fn effective_nack_after_ms(&self) -> u64 {
        if self.rtt_ms == 0 {
            NACK_AFTER_MS
        } else {
            ((self.rtt_ms as u64) / 6).clamp(4, 16)
        }
    }

    fn effective_nack_retry_ms(&self) -> u64 {
        if self.rtt_ms == 0 {
            NACK_RETRY_MS
        } else {
            ((self.rtt_ms as u64) + 12).clamp(20, 220)
        }
    }

    fn effective_give_up_ms(&self, is_key: bool) -> u64 {
        let rtt = self.rtt_ms as u64;
        if is_key {
            if rtt == 0 {
                GIVE_UP_KEY_MS
            } else {
                (rtt * 3 + 140).clamp(GIVE_UP_KEY_MS, 900)
            }
        } else if rtt == 0 {
            GIVE_UP_DELTA_MS
        } else {
            (rtt * 2 + 80).clamp(GIVE_UP_DELTA_MS, 550)
        }
    }

    fn effective_pli_interval_ms(&self) -> u64 {
        if self.rtt_ms == 0 {
            PLI_INTERVAL_MS
        } else {
            ((self.rtt_ms as u64) * 2 + 150).clamp(PLI_INTERVAL_MS, 750)
        }
    }

    pub fn push(&mut self, h: &MediaHeader, payload: &[u8], now_ms: u64) -> Vec<RxEvent> {
        self.packets_rx += 1;
        let mut ev = Vec::new();
        if h.frag_cnt == 0 {
            return ev;
        }
        if let Some(next) = self.next_id {
            if (h.frame_id.wrapping_sub(next) as i32) < 0 {
                return ev; // frame antigo / duplicado
            }
        }
        if self.need_key && !h.is_key() {
            self.dropped_ref_frames += 1;
            return ev; // sem referência: descarta silenciosamente para preservar decoder
        }

        // --- Processamento de Pacotes FEC (Paridade para Recuperação Imediata) ---
        if h.kind == KIND_FEC {
            if payload.len() < FEC_HEADER_BYTES {
                return ev;
            }
            let start_idx = u16::from_be_bytes([payload[0], payload[1]]);
            let count = u16::from_be_bytes([payload[2], payload[3]]);
            let total_video_frags = u16::from_be_bytes([payload[4], payload[5]]);
            let last_frag_len = u16::from_be_bytes([payload[6], payload[7]]);
            let parity = payload[FEC_HEADER_BYTES..].to_vec();

            let p = self.frames.entry(h.frame_id).or_insert_with(|| Partial {
                cnt: total_video_frags,
                frags: vec![None; total_video_frags as usize],
                got: 0,
                key: h.is_key(),
                ts: h.ts_90k,
                last_nack: 0,
                nacks: 0,
                fec_blocks: Vec::new(),
            });

            if !p.fec_blocks.iter().any(|b| b.start_idx == start_idx && b.count == count) {
                p.fec_blocks.push(FecBlock {
                    start_idx,
                    count,
                    total_video_frags,
                    last_frag_len,
                    parity,
                });
                let rec = p.try_fec_recovery();
                if rec > 0 {
                    self.fec_recovered += rec as u64;
                }
            }
            self.drain(now_ms, &mut ev);
            return ev;
        }

        // --- Processamento de Fragmentos de Vídeo ---
        if h.kind != KIND_VIDEO || h.frag_idx >= h.frag_cnt {
            return ev;
        }

        let p = self.frames.entry(h.frame_id).or_insert_with(|| Partial {
            cnt: h.frag_cnt,
            frags: vec![None; h.frag_cnt as usize],
            got: 0,
            key: h.is_key(),
            ts: h.ts_90k,
            last_nack: 0,
            nacks: 0,
            fec_blocks: Vec::new(),
        });
        if p.cnt != h.frag_cnt {
            return ev;
        }
        let slot = &mut p.frags[h.frag_idx as usize];
        if slot.is_none() {
            if p.nacks > 0 {
                self.nacks_recovered += 1;
                let sample_rtt = now_ms.saturating_sub(p.last_nack) as u32;
                if sample_rtt > 0 && sample_rtt < 3000 {
                    if self.rtt_ms == 0 {
                        self.rtt_ms = sample_rtt;
                    } else {
                        self.rtt_ms = (self.rtt_ms * 7 + sample_rtt) / 8;
                    }
                }
            }
            *slot = Some(payload.to_vec());
            p.got += 1;

            let rec = p.try_fec_recovery();
            if rec > 0 {
                self.fec_recovered += rec as u64;
            }
        }
        self.drain(now_ms, &mut ev);
        ev
    }

    fn drain(&mut self, now_ms: u64, ev: &mut Vec<RxEvent>) {
        if self.need_key {
            // procura o menor keyframe completo
            let key_id = self.frames.iter().find(|(_, p)| p.key && p.complete()).map(|(id, _)| *id);
            if let Some(id) = key_id {
                self.frames.retain(|fid, _| (fid.wrapping_sub(id) as i32) >= 0);
                self.next_id = Some(id);
                self.need_key = false;
            } else {
                return;
            }
        }
        let mut next = self.next_id.unwrap();

        // Fast-forward: se estamos travados num frame incompleto, mas um Keyframe (IDR) mais recente
        // JÁ chegou 100% completo na fila, pula direto para ele sem congelar esperando timeout!
        if !self.frames.get(&next).is_some_and(|p| p.complete()) {
            let newer_complete_key = self
                .frames
                .iter()
                .find(|(fid, p)| (fid.wrapping_sub(next) as i32) > 0 && p.key && p.complete())
                .map(|(fid, _)| *fid);

            if let Some(kf_id) = newer_complete_key {
                for (&fid, p) in &self.frames {
                    if (fid.wrapping_sub(kf_id) as i32) < 0 {
                        self.packets_lost += p.missing().len() as u64;
                        self.dropped_frames += 1;
                    }
                }
                self.frames.retain(|fid, _| (fid.wrapping_sub(kf_id) as i32) >= 0);
                next = kf_id;
                self.stall_since = None;
            }
        }

        while self.frames.get(&next).is_some_and(|p| p.complete()) {
            let p = self.frames.remove(&next).unwrap();
            let mut data = Vec::new();
            for f in p.frags.into_iter().flatten() {
                data.extend_from_slice(&f);
            }
            self.frames_delivered += 1;
            ev.push(RxEvent::Frame { frame_id: next, key: p.key, ts_90k: p.ts, data });
            next = next.wrapping_add(1);
            self.stall_since = None;
        }
        self.next_id = Some(next);
        if !self.frames.is_empty() && self.stall_since.is_none() {
            self.stall_since = Some(now_ms);
        }
    }

    /// Deve ser chamado periodicamente (~4 ms). Gera NACKs e pedidos de keyframe adaptados ao RTT.
    pub fn tick(&mut self, now_ms: u64) -> Vec<RxEvent> {
        let mut ev = Vec::new();
        let pli_interval = self.effective_pli_interval_ms();
        if self.need_key {
            if self.last_pli.is_none_or(|t| now_ms.saturating_sub(t) >= pli_interval) {
                self.last_pli = Some(now_ms);
                ev.push(RxEvent::RequestKey);
            }
            return ev;
        }
        let Some(next) = self.next_id else { return ev };
        let Some(since) = self.stall_since else { return ev };
        if self.frames.is_empty() {
            self.stall_since = None;
            return ev;
        }
        let is_key = self.frames.get(&next).is_some_and(|p| p.key);
        let timeout = self.effective_give_up_ms(is_key);
        let age = now_ms.saturating_sub(since);

        if age >= timeout {
            // perda irrecuperável: quebra da cadeia de referência H.264
            if let Some(stalled) = self.frames.remove(&next) {
                if !self.need_key {
                    self.packets_lost += stalled.missing().len() as u64;
                }
            }
            self.dropped_frames += 1;

            // Rastreamento estrito de continuidade:
            // Descarta todos os P-frames subsequentes até o próximo Keyframe para não quebrar a GPU
            let p_dropped = self.frames.values().filter(|p| !p.key).count() as u64;
            self.dropped_ref_frames += p_dropped;
            self.frames.retain(|_, p| p.key);

            self.need_key = true;
            self.next_id = None;
            self.stall_since = None;
            if self.last_pli.is_none_or(|t| now_ms.saturating_sub(t) >= pli_interval) {
                self.last_pli = Some(now_ms);
                ev.push(RxEvent::RequestKey);
            }
            // um keyframe já completo pode estar na fila
            self.drain(now_ms, &mut ev);
            return ev;
        }

        let nack_after = self.effective_nack_after_ms();
        let nack_retry = self.effective_nack_retry_ms();
        if age >= nack_after {
            match self.frames.get_mut(&next) {
                Some(p) => {
                    if p.nacks < 4 && (p.nacks == 0 || now_ms.saturating_sub(p.last_nack) >= nack_retry) {
                        p.nacks += 1;
                        p.last_nack = now_ms;
                        let missing = p.missing();
                        self.nacks_sent += missing.len() as u64;
                        ev.push(RxEvent::Nack { frame_id: next, missing });
                    }
                }
                None => {
                    // nenhum fragmento do frame chegou: pede o fragmento 0 para descobrir `frag_cnt`
                    let last = self.last_nack_empty.unwrap_or(0);
                    if last == 0 || now_ms.saturating_sub(last) >= nack_retry {
                        self.last_nack_empty = Some(now_ms);
                        self.nacks_sent += 1;
                        ev.push(RxEvent::Nack { frame_id: next, missing: vec![0] });
                    }
                }
            }
        }
        ev
    }

    pub fn stats(&self) -> ReassemblerStats {
        ReassemblerStats {
            packets_rx: self.packets_rx,
            packets_lost: self.packets_lost,
            nacks_sent: self.nacks_sent,
            nacks_recovered: self.nacks_recovered,
            fec_recovered: self.fec_recovered,
            dropped_frames: self.dropped_frames,
            dropped_ref_frames: self.dropped_ref_frames,
            frames_delivered: self.frames_delivered,
            rtt_ms: self.rtt_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(r: &mut Reassembler, pkts: &[Vec<u8>], now: u64) -> Vec<RxEvent> {
        let mut out = vec![];
        for p in pkts {
            let (h, pl) = MediaHeader::parse(p).unwrap();
            out.extend(r.push(&h, pl, now));
        }
        out
    }

    fn data(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_add(seed)).collect()
    }

    #[test]
    fn fragments_respect_mtu() {
        let p = packetize(KIND_VIDEO, 1, true, 0, &data(5000, 0));
        assert_eq!(p.len(), 5);
        assert!(p.iter().all(|x| x.len() <= HEADER_LEN + MAX_PAYLOAD));
        assert_eq!(packetize(KIND_VIDEO, 1, false, 0, &[]).len(), 1);
    }

    #[test]
    fn in_order_and_reordered_delivery() {
        let mut r = Reassembler::new();
        let d0 = data(3000, 1);
        let d1 = data(2500, 2);
        let mut f0 = packetize(KIND_VIDEO, 10, true, 100, &d0);
        let f1 = packetize(KIND_VIDEO, 11, false, 200, &d1);
        f0.reverse(); // fragmentos fora de ordem
        let mut ev = feed(&mut r, &f0, 0);
        ev.extend(feed(&mut r, &f1, 1));
        assert_eq!(ev.len(), 2);
        match &ev[0] { RxEvent::Frame { frame_id, key, data, .. } => { assert_eq!(*frame_id, 10); assert!(*key); assert_eq!(data, &d0); } _ => panic!() }
        match &ev[1] { RxEvent::Frame { frame_id, data, .. } => { assert_eq!(*frame_id, 11); assert_eq!(data, &d1); } _ => panic!() }
    }

    #[test]
    fn non_key_frames_dropped_until_first_keyframe() {
        let mut r = Reassembler::new();
        assert!(feed(&mut r, &packetize(KIND_VIDEO, 5, false, 0, &data(100, 0)), 0).is_empty());
        assert!(r.waiting_for_key());
        let ev = feed(&mut r, &packetize(KIND_VIDEO, 6, true, 0, &data(100, 0)), 1);
        assert_eq!(ev.len(), 1);
        assert!(!r.waiting_for_key());
    }

    #[test]
    fn nack_and_retransmit_recovers_lost_fragment() {
        let mut r = Reassembler::new();
        let mut ring = RetransmitRing::new(64);
        let d0 = data(100, 0);
        let d1 = data(4000, 3);
        let k = packetize(KIND_VIDEO, 1, true, 0, &d0);
        let p1 = packetize(KIND_VIDEO, 2, false, 0, &d1);
        for (i, p) in p1.iter().enumerate() { ring.push(2, i as u16, p.clone()); }
        assert_eq!(feed(&mut r, &k, 0).len(), 1);
        // perde o fragmento 2 do frame 2
        let lossy: Vec<_> = p1.iter().enumerate().filter(|(i, _)| *i != 2).map(|(_, p)| p.clone()).collect();
        assert!(feed(&mut r, &lossy, 1).is_empty());
        assert!(r.tick(3).is_empty()); // ainda dentro do tempo de espera (5 ms)
        let ev = r.tick(10);
        assert_eq!(ev, vec![RxEvent::Nack { frame_id: 2, missing: vec![2] }]);
        // sharer atende o NACK
        let re = ring.get(2, 2).expect("fragmento no ring");
        let out = feed(&mut r, &[(*re).clone()], 22);
        assert_eq!(out.len(), 1);
        match &out[0] { RxEvent::Frame { data, .. } => assert_eq!(data, &d1), _ => panic!() }
    }

    #[test]
    fn unrecoverable_loss_requests_keyframe_and_drops_until_key() {
        let mut r = Reassembler::new();
        feed(&mut r, &packetize(KIND_VIDEO, 1, true, 0, &data(100, 0)), 0);
        // frame 2 some inteiramente; frame 3 chega
        assert!(feed(&mut r, &packetize(KIND_VIDEO, 3, false, 0, &data(100, 0)), 1).is_empty());
        let ev = r.tick(10);
        assert_eq!(ev, vec![RxEvent::Nack { frame_id: 2, missing: vec![0] }]);
        let ev = r.tick(310);
        assert_eq!(ev, vec![RxEvent::RequestKey]);
        assert!(r.waiting_for_key());
        // frames P seguintes são ignorados; o IDR retoma
        assert!(feed(&mut r, &packetize(KIND_VIDEO, 4, false, 0, &data(100, 0)), 311).is_empty());
        let ev = feed(&mut r, &packetize(KIND_VIDEO, 5, true, 0, &data(100, 9)), 312);
        assert_eq!(ev.len(), 1);
        assert!(!r.waiting_for_key());
    }

    #[test]
    fn nack_payload_roundtrip() {
        let p = build_nack(77, &[1, 5, 300]);
        let (h, pl) = MediaHeader::parse(&p).unwrap();
        assert_eq!((h.kind, h.frame_id), (KIND_NACK, 77));
        assert_eq!(parse_nack_payload(pl), vec![1, 5, 300]);
    }

    #[test]
    fn fec_recovers_lost_fragment_instantly_without_nack() {
        let mut r = Reassembler::new();
        let d0 = data(100, 0);
        let k = packetize(KIND_VIDEO, 1, true, 0, &d0);
        assert_eq!(feed(&mut r, &k, 0).len(), 1);

        // Frame 2 com 5000 bytes (5 fragmentos)
        let d1 = data(5000, 7);
        let p1 = packetize(KIND_VIDEO, 2, false, 0, &d1);
        assert_eq!(p1.len(), 5);

        // Gera pacotes FEC (1 bloco com os 5 fragmentos)
        let fec = generate_fec(2, false, 0, &p1, 8);
        assert_eq!(fec.len(), 1);

        // Simula perda do fragmento 2 na rede (chegam 0, 1, 3, 4)
        let lossy: Vec<Vec<u8>> = p1.iter().enumerate().filter(|(i, _)| *i != 2).map(|(_, p)| p.clone()).collect();
        assert!(feed(&mut r, &lossy, 1).is_empty());

        // Agora chega o pacote FEC: ele deve reconstruir o fragmento 2 em 0ms e entregar o frame!
        let out = feed(&mut r, &fec, 2);
        assert_eq!(out.len(), 1);
        match &out[0] {
            RxEvent::Frame { frame_id, data, .. } => {
                assert_eq!(*frame_id, 2);
                assert_eq!(data, &d1);
            }
            _ => panic!("esperava entrega do frame via FEC"),
        }
        assert_eq!(r.stats().fec_recovered, 1);
        assert_eq!(r.stats().nacks_sent, 0); // Zero NACKs disparados!
    }

    #[test]
    fn congestion_controller_aimd_adjusts_bitrate() {
        let mut cc = CongestionController::new(1_000_000, 10_000_000, 5_000_000);
        assert_eq!(cc.current_bitrate(), 5_000_000);

        // Perda alta (5.0%) -> Redução multiplicativa (15%)
        let decreased = cc.on_telemetry(45, 5.0);
        assert_eq!(decreased, Some(4_250_000));
        assert_eq!(cc.current_bitrate(), 4_250_000);

        // Chamada imediata (dentro do cooldown de 1.5s) não reduz novamente
        assert_eq!(cc.on_telemetry(45, 5.0), None);

        // Reset cooldown artificialmente recriando ou testando o aumento
        let mut cc_stable = CongestionController::new(1_000_000, 5_000_000, 4_000_000);
        assert_eq!(cc_stable.on_telemetry(25, 0.0), None); // tick 1
        let increased = cc_stable.on_telemetry(25, 0.0);   // tick 2 -> +800 kbps
        assert_eq!(increased, Some(4_800_000));
        assert_eq!(cc_stable.current_bitrate(), 4_800_000);
    }

    #[test]
    fn high_rtt_link_recovers_nack_without_freezing() {
        let mut r = Reassembler::new();
        r.set_link_rtt_ms(110); // Simula conexão Internet / Relay com 110ms de ping
        let mut ring = RetransmitRing::new(128);

        let d0 = data(200, 1);
        let d1 = data(3500, 2);
        assert_eq!(feed(&mut r, &packetize(KIND_VIDEO, 1, true, 0, &d0), 0).len(), 1);

        let p1 = packetize(KIND_VIDEO, 2, false, 90, &d1);
        for (i, p) in p1.iter().enumerate() {
            ring.push(2, i as u16, p.clone());
        }
        let lossy: Vec<_> = p1.iter().enumerate().filter(|(i, _)| *i != 1).map(|(_, p)| p.clone()).collect();
        assert!(feed(&mut r, &lossy, 5).is_empty());

        // T=25ms: dispara exatamente 1 NACK
        let ev1 = r.tick(25);
        assert_eq!(ev1, vec![RxEvent::Nack { frame_id: 2, missing: vec![1] }]);

        // T=100ms (antes a versão antiga já teria desistido em 90ms e congelado a tela!):
        // Agora respeita o RTT de 110ms e nem sequer envia retry duplicado ainda!
        assert!(r.tick(100).is_empty());
        assert!(!r.waiting_for_key());

        // T=135ms: pacote retransmitido chega após 1 RTT completo e destrava o vídeo liso!
        let re = ring.get(2, 1).unwrap();
        let out = feed(&mut r, &[(*re).clone()], 135);
        assert_eq!(out.len(), 1);
        assert!(!r.waiting_for_key());
    }

    #[test]
    fn stalled_delta_fast_forwards_when_newer_keyframe_arrives() {
        let mut r = Reassembler::new();
        assert_eq!(feed(&mut r, &packetize(KIND_VIDEO, 1, true, 0, &data(200, 1)), 0).len(), 1);

        // Frame 2 (P-frame) perde um fragmento e fica travado
        let p2 = packetize(KIND_VIDEO, 2, false, 90, &data(3000, 2));
        assert!(feed(&mut r, &p2[1..], 5).is_empty());

        // Frame 3 (novo Keyframe IDR) chega completo: deve pular o frame 2 instantaneamente!
        let d3 = data(500, 9);
        let out = feed(&mut r, &packetize(KIND_VIDEO, 3, true, 180, &d3), 15);
        assert_eq!(out.len(), 1);
        match &out[0] {
            RxEvent::Frame { frame_id, key, data, .. } => {
                assert_eq!(*frame_id, 3);
                assert!(*key);
                assert_eq!(data, &d3);
            }
            _ => panic!("esperava fast-forward imediato para o Keyframe 3"),
        }
    }
}
