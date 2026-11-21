//! Ponte de captura externa (usada no Android / Meta Quest 3S via JNI).
//!
//! No Meta Quest 3S, `MediaProjection` + `QuestSingleEyeRenderer` + `MediaCodec` H.264
//! e `AudioPlaybackCapture` produzem quadros Annex-B e PCM estéreo 48 kHz em Java/Kotlin,
//! entregando diretamente para `SenderPipeline` via este módulo sem passar por Win32/D3D11.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::OnceLock;

use crossbeam_channel::Sender;
use parking_lot::Mutex;

use crate::capture::StreamKind;
use crate::video::encoder::EncodedFrame;
use crate::video::h264::{has_nal, nal_type, nals, ParamSets, NAL_IDR};

const OPUS_FRAME_SAMPLES_INTERLEAVED: usize = 960 * 2; // 20 ms @ 48 kHz estéreo = 1920 f32

struct ExternalBridgeState {
    video_tx: Mutex<HashMap<StreamKind, Sender<EncodedFrame>>>,
    param_sets: Mutex<HashMap<StreamKind, ParamSets>>,
    audio_tx: Mutex<Option<Sender<Vec<f32>>>>,
    audio_ring: Mutex<VecDeque<f32>>,
    audio_muted: AtomicBool,
    keyframe_requested: AtomicBool,
    target_bitrate_bps: AtomicU32,
    active_source_id: AtomicIsize,
}

impl ExternalBridgeState {
    fn new() -> Self {
        Self {
            video_tx: Mutex::new(HashMap::new()),
            param_sets: Mutex::new(HashMap::new()),
            audio_tx: Mutex::new(None),
            audio_ring: Mutex::new(VecDeque::with_capacity(OPUS_FRAME_SAMPLES_INTERLEAVED * 4)),
            audio_muted: AtomicBool::new(false),
            keyframe_requested: AtomicBool::new(false),
            target_bitrate_bps: AtomicU32::new(10_000_000),
            active_source_id: AtomicIsize::new(1),
        }
    }
}

fn state() -> &'static ExternalBridgeState {
    static STATE: OnceLock<ExternalBridgeState> = OnceLock::new();
    STATE.get_or_init(ExternalBridgeState::new)
}

pub fn register_video_sender(
    kind: StreamKind,
    tx: Sender<EncodedFrame>,
    bitrate_bps: u32,
    source_id: isize,
) {
    let st = state();
    st.video_tx.lock().insert(kind, tx);
    st.param_sets.lock().insert(kind, ParamSets::new(false));
    st.target_bitrate_bps.store(bitrate_bps, Ordering::Relaxed);
    st.active_source_id.store(source_id, Ordering::Relaxed);
    st.keyframe_requested.store(true, Ordering::SeqCst);
}

pub fn unregister_video_sender(kind: StreamKind) {
    let st = state();
    st.video_tx.lock().remove(&kind);
    st.param_sets.lock().remove(&kind);
}

pub fn register_audio_sender(tx: Sender<Vec<f32>>, muted: bool) {
    let st = state();
    st.audio_ring.lock().clear();
    st.audio_muted.store(muted, Ordering::SeqCst);
    *st.audio_tx.lock() = Some(tx);
}

pub fn unregister_audio_sender() {
    let st = state();
    *st.audio_tx.lock() = None;
    st.audio_ring.lock().clear();
}

pub fn set_audio_muted(muted: bool) {
    state().audio_muted.store(muted, Ordering::SeqCst);
}

pub fn request_keyframe() {
    state().keyframe_requested.store(true, Ordering::SeqCst);
}

pub fn consume_keyframe_request() -> bool {
    state().keyframe_requested.swap(false, Ordering::SeqCst)
}

pub fn set_target_bitrate(bps: u32) {
    if bps > 0 {
        state().target_bitrate_bps.store(bps, Ordering::Relaxed);
    }
}

pub fn get_target_bitrate() -> u32 {
    state().target_bitrate_bps.load(Ordering::Relaxed)
}

pub fn set_active_source_id(source_id: isize) {
    state().active_source_id.store(source_id, Ordering::Relaxed);
}

pub fn get_active_source_id() -> isize {
    state().active_source_id.load(Ordering::Relaxed)
}

/// Recebe pacotes H.264 Annex-B de um encoder externo (`MediaCodec`), guarda SPS/PPS
/// caso venham em pacote de configuração separado e injeta no canal de `SenderPipeline`.
pub fn push_external_video(kind: StreamKind, is_key: bool, pts_us: u64, annex_b: Vec<u8>) {
    if annex_b.is_empty() {
        return;
    }

    let st = state();
    let has_vcl_slice = nals(&annex_b)
        .into_iter()
        .any(|n| matches!(nal_type(n), 1..=5));

    let frame_data = {
        let mut ps_map = st.param_sets.lock();
        let ps = ps_map.entry(kind).or_insert_with(|| ParamSets::new(false));
        ps.set_from_annexb(&annex_b);
        if !has_vcl_slice {
            // Buffer apenas de configuração (SPS/PPS em separação comum do MediaCodec)
            return;
        }
        let actual_key = is_key || has_nal(&annex_b, NAL_IDR);
        if actual_key {
            ps.ensure_in_keyframe(annex_b)
        } else {
            annex_b
        }
    };

    let actual_key = is_key || has_nal(&frame_data, NAL_IDR);
    let tx_opt = st.video_tx.lock().get(&kind).cloned();
    if let Some(tx) = tx_opt {
        let pts_100ns = (pts_us as i64).saturating_mul(10);
        let _ = tx.try_send(EncodedFrame {
            data: frame_data,
            key: actual_key,
            pts_100ns,
        });
    }
}

/// Recebe PCM float estéreo 48 kHz intercalado `[L, R, ...]` e agrupa em blocos exatos de
/// 1920 amostras (`20 ms`) compatíveis com `OpusEnc::encode`.
pub fn push_external_audio_f32(samples: &[f32]) {
    if samples.is_empty() {
        return;
    }

    let st = state();
    let tx_opt = st.audio_tx.lock().clone();
    let Some(tx) = tx_opt else {
        return;
    };

    let muted = st.audio_muted.load(Ordering::Relaxed);
    let mut ring = st.audio_ring.lock();
    ring.extend(samples.iter().copied());

    while ring.len() >= OPUS_FRAME_SAMPLES_INTERLEAVED {
        if muted {
            ring.drain(..OPUS_FRAME_SAMPLES_INTERLEAVED);
            let _ = tx.try_send(vec![0.0f32; OPUS_FRAME_SAMPLES_INTERLEAVED]);
        } else {
            let mut chunk = Vec::with_capacity(OPUS_FRAME_SAMPLES_INTERLEAVED);
            chunk.extend(ring.drain(..OPUS_FRAME_SAMPLES_INTERLEAVED));
            let _ = tx.try_send(chunk);
        }
    }
}

#[cfg(target_os = "android")]
mod android_jni {
    use super::*;
    use jni::objects::{JByteArray, JClass, JFloatArray};
    use jni::sys::{jboolean, jint, jlong, JNI_TRUE};
    use jni::JNIEnv;

    #[no_mangle]
    pub extern "system" fn Java_dev_lukeshare_stream_QuestCaptureService_nativePushVideoFrame<'local>(
        env: JNIEnv<'local>,
        _class: JClass<'local>,
        is_key: jboolean,
        pts_us: jlong,
        data: JByteArray<'local>,
    ) {
        if let Ok(bytes) = env.convert_byte_array(&data) {
            push_external_video(
                StreamKind::Screen,
                is_key == JNI_TRUE,
                pts_us.max(0) as u64,
                bytes,
            );
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_dev_lukeshare_stream_QuestCaptureService_nativePushAudioPcm<'local>(
        env: JNIEnv<'local>,
        _class: JClass<'local>,
        samples: JFloatArray<'local>,
        count: jint,
    ) {
        if count <= 0 {
            return;
        }
        let n = count as usize;
        let mut buf = vec![0.0f32; n];
        if env.get_float_array_region(&samples, 0, &mut buf).is_ok() {
            push_external_audio_f32(&buf);
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_dev_lukeshare_stream_QuestCaptureService_nativeConsumeKeyframeRequest<'local>(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
    ) -> jboolean {
        if consume_keyframe_request() {
            1
        } else {
            0
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_dev_lukeshare_stream_QuestCaptureService_nativeGetTargetBitrate<'local>(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
    ) -> jint {
        get_target_bitrate().min(i32::MAX as u32) as jint
    }

    #[no_mangle]
    pub extern "system" fn Java_dev_lukeshare_stream_QuestCaptureService_nativeGetActiveEyeMode<'local>(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
    ) -> jint {
        get_active_source_id().clamp(1, 4) as jint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    fn test_guard() -> parking_lot::MutexGuard<'static, ()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(())).lock()
    }

    #[test]
    fn external_audio_chunks_into_exact_20ms_1920_float_frames() {
        let _g = test_guard();
        let (tx, rx) = bounded(8);
        register_audio_sender(tx, false);

        // Envia metade (960 amostras) — ainda não deve emitir bloco
        push_external_audio_f32(&vec![0.25f32; 960]);
        assert!(rx.try_recv().is_err());

        // Completa + ultrapassa: total 960 + 3000 = 3960 -> 2 blocos de 1920 (sobram 120 no anel)
        push_external_audio_f32(&vec![0.5f32; 3000]);
        let b1 = rx.try_recv().expect("primeiro chunk 20ms");
        let b2 = rx.try_recv().expect("segundo chunk 20ms");
        assert_eq!(b1.len(), 1920);
        assert_eq!(b2.len(), 1920);
        assert!(rx.try_recv().is_err());

        // Testa modo mudo -> entrega silêncio de 1920 amostras
        set_audio_muted(true);
        push_external_audio_f32(&vec![0.9f32; 1920]);
        let muted_chunk = rx.try_recv().expect("chunk mutado");
        assert_eq!(muted_chunk.len(), 1920);
        assert!(muted_chunk.iter().all(|&s| s == 0.0));

        unregister_audio_sender();
    }

    #[test]
    fn external_video_caches_sps_pps_and_prepends_to_idr_keyframe() {
        let _g = test_guard();
        let (tx, rx) = bounded(8);
        register_video_sender(StreamKind::Screen, tx, 8_000_000, 1);

        // 1. Pacote CSD separado apenas com SPS (type 7) e PPS (type 8)
        let csd = vec![0, 0, 0, 1, 0x67, 0x64, 0x00, 0x2A, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80];
        push_external_video(StreamKind::Screen, false, 1000, csd);
        assert!(rx.try_recv().is_err(), "pacote apenas SPS/PPS não deve ser enviado sem slice VCL");

        // 2. IDR separado (type 5) sem SPS/PPS no cabeçalho
        let idr = vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21];
        push_external_video(StreamKind::Screen, true, 2000, idr);

        let frame = rx.try_recv().expect("IDR deve chegar com SPS/PPS prefixado");
        assert!(frame.key);
        assert_eq!(frame.pts_100ns, 20_000);
        assert!(has_nal(&frame.data, 7), "SPS deve estar presente no keyframe");
        assert!(has_nal(&frame.data, 8), "PPS deve estar presente no keyframe");
        assert!(has_nal(&frame.data, NAL_IDR), "IDR deve estar presente");

        unregister_video_sender(StreamKind::Screen);
    }
}
