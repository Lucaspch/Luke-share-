//! Transporte de mídia sobre UDP: fragmentação, remontagem com NACK / pedido de keyframe e
//! ring buffer de retransmissão. Toda a lógica de tempo recebe `now_ms` (testável sem relógio).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

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

struct Partial {
    cnt: u16,
    frags: Vec<Option<Vec<u8>>>,
    got: u16,
    key: bool,
    ts: u32,
    last_nack: u64,
    nacks: u8,
}

impl Partial {
    fn complete(&self) -> bool {
        self.got == self.cnt
    }
    fn missing(&self) -> Vec<u16> {
        self.frags.iter().enumerate().filter(|(_, f)| f.is_none()).map(|(i, _)| i as u16).collect()
    }
}

pub const NACK_AFTER_MS: u64 = 4;
pub const NACK_RETRY_MS: u64 = 15;
pub const GIVE_UP_DELTA_MS: u64 = 90;
pub const GIVE_UP_KEY_MS: u64 = 140;
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
            dropped_frames: 0,
            dropped_ref_frames: 0,
            frames_delivered: 0,
            rtt_ms: 0,
        }
    }

    pub fn waiting_for_key(&self) -> bool {
        self.need_key
    }

    pub fn push(&mut self, h: &MediaHeader, payload: &[u8], now_ms: u64) -> Vec<RxEvent> {
        self.packets_rx += 1;
        let mut ev = Vec::new();
        if h.kind != KIND_VIDEO || h.frag_cnt == 0 || h.frag_idx >= h.frag_cnt {
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
        let p = self.frames.entry(h.frame_id).or_insert_with(|| Partial {
            cnt: h.frag_cnt,
            frags: vec![None; h.frag_cnt as usize],
            got: 0,
            key: h.is_key(),
            ts: h.ts_90k,
            last_nack: 0,
            nacks: 0,
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

    /// Deve ser chamado periodicamente (~4 ms). Gera NACKs e pedidos de keyframe.
    pub fn tick(&mut self, now_ms: u64) -> Vec<RxEvent> {
        let mut ev = Vec::new();
        if self.need_key {
            if self.last_pli.is_none_or(|t| now_ms.saturating_sub(t) >= PLI_INTERVAL_MS) {
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
        let is_key = self.frames.get(&next).map_or(false, |p| p.key);
        let timeout = if is_key { GIVE_UP_KEY_MS } else { GIVE_UP_DELTA_MS };
        let age = now_ms.saturating_sub(since);

        if age >= timeout {
            // perda irrecuperável: quebra da cadeia de referência H.264
            if let Some(stalled) = self.frames.remove(&next) {
                self.packets_lost += stalled.missing().len() as u64;
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
            if self.last_pli.is_none_or(|t| now_ms.saturating_sub(t) >= PLI_INTERVAL_MS) {
                self.last_pli = Some(now_ms);
                ev.push(RxEvent::RequestKey);
            }
            // um keyframe já completo pode estar na fila
            self.drain(now_ms, &mut ev);
            return ev;
        }
        if age >= NACK_AFTER_MS {
            match self.frames.get_mut(&next) {
                Some(p) => {
                    if p.nacks < 3 && (p.nacks == 0 || now_ms.saturating_sub(p.last_nack) >= NACK_RETRY_MS) {
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
                    if last == 0 || now_ms.saturating_sub(last) >= NACK_RETRY_MS {
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
}
