//! Encoder H.264 via Media Foundation.
//!
//! Prioriza MFT de **hardware** (NVENC / AMF / QuickSync) com entrada em textura D3D11 NV12
//! (zero-copy) e cai para o MFT de software da Microsoft (com readback da GPU) se necessário.
//! Saída em Annex B com SPS/PPS garantidos em todo keyframe.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoTaskMemFree, CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use serde::{Deserialize, Serialize};

use super::d3d::Gpu;
use super::h264::{
    has_nal, hevc_nal_type, nals, ParamSets, NAL_HEVC_CRA, NAL_HEVC_IDR_N_LP, NAL_HEVC_IDR_W_RADL,
    NAL_IDR,
};

fn var_bool(b: bool) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_BOOL;
        (*v.Anonymous.Anonymous).Anonymous.boolVal = if b {
            windows::Win32::Foundation::VARIANT_TRUE
        } else {
            windows::Win32::Foundation::VARIANT_FALSE
        };
    }
    v
}

fn var_u32(n: u32) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_UI4;
        (*v.Anonymous.Anonymous).Anonymous.ulVal = n;
    }
    v
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VideoCodec {
    #[default]
    Auto,
    Av1,
    Hevc,
    H264,
}

impl VideoCodec {
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoCodec::Auto => "auto",
            VideoCodec::Av1 => "av1",
            VideoCodec::Hevc => "hevc",
            VideoCodec::H264 => "h264",
        }
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s.to_lowercase().trim() {
            "av1" => VideoCodec::Av1,
            "hevc" | "h265" | "h.265" => VideoCodec::Hevc,
            "h264" | "avc" | "h.264" => VideoCodec::H264,
            _ => VideoCodec::Auto,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub codec: VideoCodec,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub key: bool,
    pub pts_100ns: i64,
}

struct Job {
    tex: ID3D11Texture2D,
    pts: i64,
}
unsafe impl Send for Job {}

pub struct H264Encoder {
    tx: Sender<Job>,
    force_key: Arc<AtomicBool>,
    new_bitrate: Arc<std::sync::atomic::AtomicU32>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Nome amigável do MFT escolhido (ex.: "NVIDIA H.264 Encoder MFT").
    pub name: String,
    pub hardware: bool,
    pub codec: VideoCodec,
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

struct Core {
    t: IMFTransform,
    api: Option<ICodecAPI>,
    async_mft: bool,
    hardware: bool,
    name: String,
    cfg: EncoderConfig,
    gpu: Gpu,
    staging: Option<ID3D11Texture2D>,
    provides_samples: bool,
    out_size: u32,
    actual_codec: VideoCodec,
}

impl Core {
    fn new_with(
        activate: &IMFActivate,
        hardware: bool,
        mgr: &IMFDXGIDeviceManager,
        gpu: &Gpu,
        cfg: &EncoderConfig,
        actual_codec: VideoCodec,
    ) -> Result<Core> {
        unsafe {
            let name = {
                let mut p = windows::core::PWSTR::null();
                let mut n = 0u32;
                match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut n) {
                    Ok(()) => {
                        let s = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.0 as *const _));
                        s
                    }
                    Err(_) => format!("{:?} Encoder", actual_codec),
                }
            };
            let t: IMFTransform = activate.ActivateObject().context("ActivateObject")?;
            let attrs = t.GetAttributes().ok();
            let mut async_mft = false;
            if let Some(a) = &attrs {
                if a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1 {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("ASYNC_UNLOCK")?;
                    async_mft = true;
                }
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if hardware {
                t.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize).context("SET_D3D_MANAGER")?;
            }

            // --- tipo de saída (H.264, HEVC ou AV1) ---
            let ot = MFCreateMediaType()?;
            ot.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            match actual_codec {
                VideoCodec::H264 => {
                    ot.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                    let _ = ot.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32);
                }
                VideoCodec::Hevc => {
                    ot.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
                }
                VideoCodec::Av1 => {
                    ot.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_AV1)?;
                }
                VideoCodec::Auto => unreachable!(),
            }
            ot.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
            ot.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
            ot.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
            ot.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            ot.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetOutputType(0, &ot, 0).context("SetOutputType")?;

            // --- tipo de entrada (NV12, BT.709 limitado) ---
            let it = MFCreateMediaType()?;
            it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            it.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
            it.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
            it.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            it.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let _ = it.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
            let _ = it.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
            let _ = it.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            let _ = it.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            t.SetInputType(0, &it, 0).context("SetInputType")?;

            // --- ajustes de baixa latência ---
            let api: Option<ICodecAPI> = t.cast().ok();
            if let Some(api) = &api {
                let set = |k: &GUID, v: VARIANT| {
                    if let Err(e) = api.SetValue(k, &v) {
                        tracing::debug!("ICodecAPI.SetValue falhou: {e}");
                    }
                };
                set(&CODECAPI_AVLowLatencyMode, var_bool(true));
                set(&CODECAPI_AVEncCommonRateControlMode, var_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
                set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(cfg.bitrate));
                set(&CODECAPI_AVEncCommonMaxBitRate, var_u32(cfg.bitrate.saturating_mul(125) / 100));
                set(&CODECAPI_AVEncCommonQuality, var_u32(85));
                set(&CODECAPI_AVEncCommonQualityVsSpeed, var_u32(75));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, var_u32(0));
                set(&CODECAPI_AVEncMPVGOPSize, var_u32((cfg.fps * 2).max(60))); // 120 frames @ 60fps = 2 segundos
                set(&CODECAPI_AVEncCommonRealTime, var_u32(1));
            }

            let info = t.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0
                || info.dwFlags & (MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            t.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Core {
                t,
                api,
                async_mft,
                hardware,
                name,
                cfg: cfg.clone(),
                gpu: gpu.clone(),
                staging: None,
                provides_samples: provides,
                out_size: info.cbSize.max(1 << 20),
                actual_codec,
            })
        }
    }

    fn build_input_sample(&mut self, tex: &ID3D11Texture2D, pts: i64) -> Result<IMFSample> {
        unsafe {
            let sample = MFCreateSample()?;
            let buf: IMFMediaBuffer = if self.hardware {
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?
            } else {
                self.readback_nv12(tex)?
            };
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(10_000_000 / self.cfg.fps.max(1) as i64)?;
            Ok(sample)
        }
    }

    /// Caminho de software: copia a textura NV12 para memória do sistema.
    fn readback_nv12(&mut self, tex: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        unsafe {
            let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
            if self.staging.is_none() {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut d);
                d.Usage = D3D11_USAGE_STAGING;
                d.BindFlags = 0;
                d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                d.MiscFlags = 0;
                let mut s = None;
                self.gpu.device.CreateTexture2D(&d, None, Some(&mut s))?;
                self.staging = s;
            }
            let st = self.staging.as_ref().unwrap();
            self.gpu.context.CopyResource(st, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.gpu.context.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let total = w * h * 3 / 2;
            let buf = MFCreateMemoryBuffer(total as u32)?;
            let mut dst = std::ptr::null_mut();
            buf.Lock(&mut dst, None, None)?;
            let pitch = m.RowPitch as usize;
            let src = m.pData as *const u8;
            for y in 0..h {
                std::ptr::copy_nonoverlapping(src.add(y * pitch), dst.add(y * w), w);
            }
            let uv_src = src.add(pitch * h);
            for y in 0..h / 2 {
                std::ptr::copy_nonoverlapping(uv_src.add(y * pitch), dst.add(w * h + y * w), w);
            }
            buf.Unlock()?;
            buf.SetCurrentLength(total as u32)?;
            self.gpu.context.Unmap(st, 0);
            Ok(buf)
        }
    }

    fn force_keyframe(&self) {
        if let Some(api) = &self.api {
            unsafe {
                let v = var_u32(1);
                let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v);
            }
        }
    }

    fn set_bitrate(&mut self, br: u32) {
        if let Some(api) = &self.api {
            unsafe {
                let v = var_u32(br);
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v);
                let max_br = var_u32(br.saturating_mul(125) / 100);
                let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &max_br);
            }
            self.cfg.bitrate = br;
            tracing::info!("encoder: bitrate atualizado dinamicamente para {} bps", br);
        }
    }

    /// Lê uma amostra de saída; Ok(None) se o MFT precisa de mais entrada.
    fn process_output(&mut self) -> Result<Option<(Vec<u8>, bool, i64)>> {
        unsafe {
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            if !self.provides_samples {
                let s = MFCreateSample()?;
                let b = MFCreateMemoryBuffer(self.out_size)?;
                s.AddBuffer(&b)?;
                out[0].pSample = ManuallyDrop::new(Some(s));
            }
            let mut status = 0u32;
            let r = self.t.ProcessOutput(0, &mut out, &mut status);
            let sample = ManuallyDrop::take(&mut out[0].pSample);
            let _events = ManuallyDrop::take(&mut out[0].pEvents);
            match r {
                Ok(()) => {
                    let Some(sample) = sample else { return Ok(None) };
                    let buf = sample.ConvertToContiguousBuffer()?;
                    let mut p = std::ptr::null_mut();
                    let mut len = 0u32;
                    buf.Lock(&mut p, None, Some(&mut len))?;
                    let data = std::slice::from_raw_parts(p, len as usize).to_vec();
                    buf.Unlock()?;
                    let pts = sample.GetSampleTime().unwrap_or(0);
                    let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                    Ok(Some((data, clean, pts)))
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if let Ok(t) = self.t.GetOutputAvailableType(0, 0) {
                        let _ = self.t.SetOutputType(0, &t, 0);
                    }
                    Ok(None)
                }
                Err(e) => Err(e.into()),
            }
        }
    }
}

fn enumerate(hardware: bool, codec: VideoCodec) -> Vec<IMFActivate> {
    unsafe {
        let subtype = match codec {
            VideoCodec::H264 => MFVideoFormat_H264,
            VideoCodec::Hevc => MFVideoFormat_HEVC,
            VideoCodec::Av1 => MFVideoFormat_AV1,
            VideoCodec::Auto => MFVideoFormat_H264,
        };
        let in_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let out_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
        let kind = if hardware { MFT_ENUM_FLAG_HARDWARE.0 } else { MFT_ENUM_FLAG_SYNCMFT.0 };
        let flags = MFT_ENUM_FLAG(kind | MFT_ENUM_FLAG_SORTANDFILTER.0);
        let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&in_info), Some(&out_info), &mut pp, &mut count).is_err() || pp.is_null() {
            return vec![];
        }
        let mut v = Vec::new();
        for i in 0..count as usize {
            if let Some(a) = std::ptr::read(pp.add(i)) {
                v.push(a);
            }
        }
        CoTaskMemFree(Some(pp as *const _));
        v
    }
}

fn build_core(gpu: &Gpu, cfg: &EncoderConfig, allow_software: bool) -> Result<Core> {
    unsafe {
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
        let mgr = mgr.ok_or_else(|| anyhow!("IMFDXGIDeviceManager nulo"))?;
        mgr.ResetDevice(&gpu.device, token)?;

        let candidates = match cfg.codec {
            VideoCodec::Auto => vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
            VideoCodec::Av1 => vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
            VideoCodec::Hevc => vec![VideoCodec::Hevc, VideoCodec::H264],
            VideoCodec::H264 => vec![VideoCodec::H264],
        };

        let mut errs = Vec::new();
        for &codec in &candidates {
            for a in enumerate(true, codec) {
                match Core::new_with(&a, true, &mgr, gpu, cfg, codec) {
                    Ok(c) => return Ok(c),
                    Err(e) => errs.push(format!("hw ({codec:?}): {e:#}")),
                }
            }
        }
        if allow_software {
            for &codec in &candidates {
                for a in enumerate(false, codec) {
                    match Core::new_with(&a, false, &mgr, gpu, cfg, codec) {
                        Ok(c) => return Ok(c),
                        Err(e) => errs.push(format!("sw ({codec:?}): {e:#}")),
                    }
                }
            }
        }
        Err(anyhow!("nenhum encoder de vídeo utilizável: {}", errs.join(" | ")))
    }
}

impl H264Encoder {
    /// Inicia o encoder. `on_frame` é chamado na thread do encoder para cada frame codificado.
    pub fn start(
        gpu: &Gpu,
        cfg: EncoderConfig,
        allow_software: bool,
        mut on_frame: Box<dyn FnMut(EncodedFrame) + Send>,
    ) -> Result<Self> {
        let (tx, rx) = bounded::<Job>(2);
        let force_key = Arc::new(AtomicBool::new(true));
        let new_bitrate = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = bounded::<Result<(String, bool, VideoCodec), String>>(1);
        let (fk, nb, st, gpu2) = (force_key.clone(), new_bitrate.clone(), stop.clone(), gpu.clone());
        let thread = std::thread::Builder::new().name("video-encoder".into()).spawn(move || unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
            match build_core(&gpu2, &cfg, allow_software) {
                Ok(mut core) => {
                    let actual_codec = core.actual_codec;
                    let _ = ready_tx.send(Ok((core.name.clone(), core.hardware, actual_codec)));
                    run_loop(&mut core, &rx, &fk, &nb, &st, &mut on_frame);
                    let _ = core.t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                    let _ = core.t.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
            }
            let _ = MFShutdown();
            if com {
                CoUninitialize();
            }
        })?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok((name, hardware, codec))) => Ok(Self { tx, force_key, new_bitrate, stop, thread: Some(thread), name, hardware, codec }),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("timeout iniciando o encoder")),
        }
    }

    /// Enfileira uma textura NV12. Retorna false se o encoder está ocupado (frame descartado).
    pub fn submit(&self, tex: ID3D11Texture2D, pts_100ns: i64) -> bool {
        match self.tx.try_send(Job { tex, pts: pts_100ns }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::SeqCst);
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        self.new_bitrate.store(bitrate, Ordering::Relaxed);
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn emit(core: &mut Core, params: &mut ParamSets, on_frame: &mut (dyn FnMut(EncodedFrame) + Send)) -> Result<bool> {
    match core.process_output()? {
        Some((data, clean, pts)) => {
            let key = clean || match core.actual_codec {
                VideoCodec::H264 => has_nal(&data, NAL_IDR),
                VideoCodec::Hevc => nals(&data).iter().any(|n| {
                    let t = hevc_nal_type(n);
                    t == NAL_HEVC_IDR_W_RADL || t == NAL_HEVC_IDR_N_LP || t == NAL_HEVC_CRA
                }),
                VideoCodec::Av1 => clean,
                VideoCodec::Auto => clean,
            };
            let data = if key && core.actual_codec != VideoCodec::Av1 {
                params.ensure_in_keyframe(data)
            } else {
                data
            };
            on_frame(EncodedFrame { data, key, pts_100ns: pts });
            Ok(true)
        }
        None => Ok(false),
    }
}

fn run_loop(
    core: &mut Core,
    rx: &Receiver<Job>,
    force_key: &AtomicBool,
    new_bitrate: &std::sync::atomic::AtomicU32,
    stop: &AtomicBool,
    on_frame: &mut (dyn FnMut(EncodedFrame) + Send),
) {
    let mut params = ParamSets::new(core.actual_codec == VideoCodec::Hevc);
    let evgen: Option<IMFMediaEventGenerator> = if core.async_mft { core.t.cast().ok() } else { None };
    let mut need_input = 0u32;
    let mut pts_base: Option<i64> = None;
    tracing::info!("encoder '{}' ({}), {}x{} @{} fps, {} bps", core.name, if core.hardware { "hardware" } else { "software" }, core.cfg.width, core.cfg.height, core.cfg.fps, core.cfg.bitrate);

    let feed = |core: &mut Core, job: Job, pts_base: &mut Option<i64>, force_key: &AtomicBool| -> Result<()> {
        let base = *pts_base.get_or_insert(job.pts);
        if force_key.swap(false, Ordering::SeqCst) {
            core.force_keyframe();
        }
        let sample = core.build_input_sample(&job.tex, job.pts - base)?;
        unsafe { core.t.ProcessInput(0, &sample, 0)? };
        Ok(())
    };
    // descarta frames antigos: fica só com o mais novo (prioridade = latência)
    let newest = |rx: &Receiver<Job>| -> Option<Job> {
        let mut last = None;
        while let Ok(j) = rx.try_recv() {
            last = Some(j);
        }
        last
    };

    while !stop.load(Ordering::SeqCst) {
        let pending_br = new_bitrate.swap(0, Ordering::Relaxed);
        if pending_br > 0 {
            core.set_bitrate(pending_br);
        }

        if let Some(gen) = &evgen {
            // ---- MFT assíncrono (hardware): dirigido por eventos ----
            let mut worked = false;
            loop {
                match unsafe { gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(ev) => {
                        worked = true;
                        let ty = unsafe { ev.GetType().unwrap_or(0) };
                        if ty == METransformNeedInput.0 as u32 {
                            need_input += 1;
                        } else if ty == METransformHaveOutput.0 as u32 {
                            if let Err(e) = emit(core, &mut params, on_frame) {
                                tracing::warn!("encoder: ProcessOutput falhou: {e:#}");
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            if need_input > 0 {
                let job = match rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(j) => Some(newest(rx).unwrap_or(j)),
                    Err(_) => None,
                };
                if let Some(job) = job {
                    match feed(core, job, &mut pts_base, force_key) {
                        Ok(()) => need_input -= 1,
                        Err(e) => tracing::warn!("encoder: ProcessInput falhou: {e:#}"),
                    }
                }
            } else if !worked {
                std::thread::sleep(Duration::from_millis(1));
            }
        } else {
            // ---- MFT síncrono (software) ----
            if let Ok(j) = rx.recv_timeout(Duration::from_millis(20)) {
                let job = newest(rx).unwrap_or(j);
                match feed(core, job, &mut pts_base, force_key) {
                    Ok(()) => {
                        while let Ok(true) = emit(core, &mut params, on_frame) {}
                    }
                    Err(e) => tracing::warn!("encoder: ProcessInput falhou: {e:#}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codecs_constants() {
        let _ = MFVideoFormat_H264;
        let _ = MFVideoFormat_HEVC;
        let _ = MFVideoFormat_AV1;
    }
}
