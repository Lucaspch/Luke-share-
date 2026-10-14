//! WASAPI Application Loopback (Windows 10 build 20348+ / Windows 11).
//!
//! `ActivateAudioInterfaceAsync` com `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` e
//! `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`:
//!  * `ExcludeTree(pid)` → `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`
//!  * `IncludeTree(pid)` → `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`
//!
//! Cada captura roda em sua thread e alimenta um ring de amostras f32 (48 kHz estéreo
//! intercalado). Quem consome (o `AudioCapturer`) dita o relógio de 20 ms.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use windows::core::{implement, Interface, Ref, HRESULT};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IAgileObject, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms por quadro.
pub const FRAME_SAMPLES: usize = 960;
/// Limite do ring por fonte (~120 ms) para impedir acúmulo de latência.
const RING_MAX: usize = SAMPLE_RATE as usize * CHANNELS * 120 / 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopbackMode {
    ExcludeTree(u32),
    IncludeTree(u32),
}

/// Inicializa COM (MTA) na thread corrente; desfaz ao sair do escopo.
pub struct ComGuard(bool);
impl ComGuard {
    pub fn mta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ComGuard(hr.is_ok())
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() }
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Done(crossbeam_channel::Sender<()>);

impl windows::Win32::System::Com::IAgileObject_Impl for Done_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
    fn ActivateCompleted(&self, _op: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    ready: HANDLE,
    is_float: bool,
}

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.ready);
        }
    }
}

fn activate(mode: LoopbackMode) -> Result<IAudioClient> {
    let (pid, m) = match mode {
        LoopbackMode::ExcludeTree(p) => (p, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
        LoopbackMode::IncludeTree(p) => (p, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
    };
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: m },
        },
    };
    // ATENÇÃO: o PROPVARIANT aponta para memória da pilha; o Drop dele chamaria PropVariantClear →
    // CoTaskMemFree(blob) e corromperia o heap. Por isso fica em ManuallyDrop (nunca é liberado).
    let pv = std::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &mut params as *mut _ as *mut u8,
                    },
                },
            }),
        },
    });
    let (tx, rx) = crossbeam_channel::bounded(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)
    }
    .context("ActivateAudioInterfaceAsync")?;
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow!("timeout ativando loopback de processo"))?;
    let mut hr = HRESULT(0);
    let mut unk: Option<windows::core::IUnknown> = None;
    unsafe { op.GetActivateResult(&mut hr, &mut unk) }.context("GetActivateResult")?;
    hr.ok().context("ativação do loopback de processo falhou")?;
    let client: IAudioClient = unk.ok_or_else(|| anyhow!("IAudioClient nulo"))?.cast()?;
    Ok(client)
}

fn open(mode: LoopbackMode) -> Result<Opened> {
    let client = activate(mode)?;
    // O loopback por processo não suporta GetMixFormat: o formato é imposto por nós.
    let flt = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let pcm = WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let base = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let attempts: [(&WAVEFORMATEX, u32, bool); 3] = [
        (&flt, base | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, true),
        (&pcm, base | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, false),
        (&pcm, base, false),
    ];
    let mut last_err = None;
    let mut is_float = true;
    let mut current = client;
    for (i, (fmt, flags, f)) in attempts.iter().enumerate() {
        let r = unsafe { current.Initialize(AUDCLNT_SHAREMODE_SHARED, *flags, 200_000, 0, *fmt, None) };
        match r {
            Ok(()) => {
                is_float = *f;
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                if i + 1 < attempts.len() {
                    // um IAudioClient só pode ser inicializado uma vez: reativa
                    current = activate(mode)?;
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(anyhow!("IAudioClient::Initialize falhou: {e}"));
    }
    let ready = unsafe { CreateEventW(None, false, false, None)? };
    unsafe { current.SetEventHandle(ready)? };
    let capture: IAudioCaptureClient = unsafe { current.GetService()? };
    unsafe { current.Start()? };
    Ok(Opened { client: current, capture, ready, is_float })
}

/// Uma captura WASAPI de loopback por processo, rodando em thread própria.
pub struct LoopbackCapture {
    pub mode: LoopbackMode,
    pub ring: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackCapture {
    pub fn start(mode: LoopbackMode) -> Result<Self> {
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_MAX)));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);
        let (r2, s2) = (ring.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("wasapi-loopback-{mode:?}"))
            .spawn(move || {
                let _com = ComGuard::mta();
                let opened = match open(mode) {
                    Ok(o) => {
                        let _ = ready_tx.send(Ok(()));
                        o
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                capture_loop(&opened, &r2, &s2);
            })?;
        match ready_rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(())) => Ok(Self { mode, ring, stop, thread: Some(thread) }),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("timeout iniciando captura de áudio")),
        }
    }

    /// Remove até um quadro de 20 ms (960 amostras estéreo) do ring, completando com silêncio.
    pub fn pop_frame(&self, out: &mut [f32]) {
        let mut r = self.ring.lock();
        for o in out.iter_mut() {
            *o = r.pop_front().unwrap_or(0.0);
        }
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn capture_loop(o: &Opened, ring: &Mutex<VecDeque<f32>>, stop: &AtomicBool) {
    unsafe {
        while !stop.load(Ordering::SeqCst) {
            let w = WaitForSingleObject(o.ready, 50);
            if w != WAIT_OBJECT_0 {
                continue; // nada tocando: o loopback não entrega pacotes (silêncio é preenchido pelo consumidor)
            }
            loop {
                let Ok(avail) = o.capture.GetNextPacketSize() else { break };
                if avail == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if o.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break;
                }
                let n = frames as usize * CHANNELS;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null();
                {
                    let mut r = ring.lock();
                    if silent {
                        r.extend(std::iter::repeat(0.0f32).take(n));
                    } else if o.is_float {
                        let s = std::slice::from_raw_parts(data as *const f32, n);
                        r.extend(s.iter().copied());
                    } else {
                        let s = std::slice::from_raw_parts(data as *const i16, n);
                        r.extend(s.iter().map(|v| *v as f32 / 32768.0));
                    }
                    while r.len() > RING_MAX {
                        r.pop_front();
                    }
                }
                let _ = o.capture.ReleaseBuffer(frames);
            }
        }
    }
}

/// PIDs com sessão de áudio **ativa** no dispositivo de renderização padrão.
/// Usado apenas no caso multi-raiz do Discord (mixer de fallback).
pub fn active_session_pids() -> Vec<u32> {
    let _com = ComGuard::mta();
    let mut out = Vec::new();
    let r: windows::core::Result<()> = (|| unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
        let sessions = mgr.GetSessionEnumerator()?;
        for i in 0..sessions.GetCount()? {
            let c = sessions.GetSession(i)?;
            if c.GetState()? != AudioSessionStateActive {
                continue;
            }
            let c2: IAudioSessionControl2 = c.cast()?;
            let pid = c2.GetProcessId()?;
            if pid != 0 {
                out.push(pid);
            }
        }
        Ok(())
    })();
    let _ = r;
    out
}
