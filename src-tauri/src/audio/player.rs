//! Reprodutor de áudio nativo WASAPI (Shared Mode, 48 kHz Estéreo Float32).
//!
//! Executa em thread dedicada de alta prioridade no Rust (fora do WebView2),
//! eliminando engasgos ou suspensão de áudio quando a janela está minimizada ou em segundo plano,
//! além de remover 100% da serialização Base64/IPC de áudio.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::loopback::{ComGuard, CHANNELS, SAMPLE_RATE};

static ACTIVE_PLAYERS: AtomicUsize = AtomicUsize::new(0);

/// Quantidade de streams reproduzindo áudio via WASAPI neste processo no momento.
/// Usado pelo `AudioCapturer` para garantir que o áudio assistido nunca gere eco na própria live.
pub fn active_audio_players_count() -> usize {
    ACTIVE_PLAYERS.load(Ordering::Relaxed)
}

const MIN_PREBUFFER_FRAMES: usize = (SAMPLE_RATE as usize * 45) / 1000; // 45 ms (2160 frames estéreo)
const MAX_PREBUFFER_FRAMES: usize = (SAMPLE_RATE as usize * 85) / 1000; // 85 ms (4080 frames estéreo)
const HARD_CAP_FRAMES: usize = (SAMPLE_RATE as usize * 260) / 1000;     // 260 ms teto máximo anti-atraso
const FADE_IN_FRAMES: usize = 96;                                       // 2 ms micro fade-in anti-estalo

struct RenderDevice {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
}

impl Drop for RenderDevice {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn open_default_render() -> Result<RenderDevice> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("MMDeviceEnumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("GetDefaultAudioEndpoint(eRender)")?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("Activate IAudioClient")?;

        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * (CHANNELS as u32) * 4,
            nBlockAlign: (CHANNELS as u16) * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

        // 40 ms de buffer compartilhado no mixer do Windows
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 400_000, 0, &fmt, None)
            .context("IAudioClient::Initialize render")?;

        let event = CreateEventW(None, false, false, None).context("CreateEventW render")?;
        client.SetEventHandle(event).context("SetEventHandle render")?;

        let buffer_frames = client.GetBufferSize().context("GetBufferSize render")?;
        let render: IAudioRenderClient = client.GetService().context("GetService IAudioRenderClient")?;
        client.Start().context("IAudioClient::Start render")?;

        Ok(RenderDevice {
            client,
            render,
            event,
            buffer_frames,
        })
    }
}

pub struct NativeAudioPlayer {
    ring: Arc<Mutex<VecDeque<f32>>>,
    volume_bits: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NativeAudioPlayer {
    pub fn start() -> Self {
        ACTIVE_PLAYERS.fetch_add(1, Ordering::SeqCst);

        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(HARD_CAP_FRAMES * CHANNELS)));
        let volume_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let muted = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let r_clone = ring.clone();
        let v_clone = volume_bits.clone();
        let m_clone = muted.clone();
        let s_clone = stop.clone();

        let thread = std::thread::Builder::new()
            .name("wasapi-native-player".into())
            .spawn(move || {
                let _com = ComGuard::mta();
                unsafe {
                    let _ = windows::Win32::Media::timeBeginPeriod(1);
                }

                let mut dev_opt = match open_default_render() {
                    Ok(d) => Some(d),
                    Err(e) => {
                        tracing::warn!("audio_player: sem dispositivo de saída WASAPI inicial: {e:#}");
                        None
                    }
                };

                let mut buffering = true;
                let mut target_prebuffer = MIN_PREBUFFER_FRAMES;
                let mut fade_pos: usize = 0;
                let mut last_underrun = Instant::now() - Duration::from_secs(10);
                let mut last_reopen_try = Instant::now() - Duration::from_secs(5);
                let mut drift_step: usize = 0;

                while !s_clone.load(Ordering::SeqCst) {
                    if dev_opt.is_none() {
                        if last_reopen_try.elapsed() >= Duration::from_secs(2) {
                            last_reopen_try = Instant::now();
                            if let Ok(d) = open_default_render() {
                                tracing::info!("audio_player: dispositivo WASAPI reconectado com sucesso!");
                                dev_opt = Some(d);
                            }
                        }
                        // Drena excesso se não houver fone/caixa conectada para não acumular memória
                        {
                            let mut q = r_clone.lock();
                            let max_samples = MAX_PREBUFFER_FRAMES * CHANNELS;
                            if q.len() > max_samples {
                                let drop_n = q.len() - max_samples;
                                q.drain(..drop_n);
                            }
                        }
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }

                    let dev = dev_opt.as_ref().unwrap();
                    let wait = unsafe { WaitForSingleObject(dev.event, 15) };
                    if wait != WAIT_OBJECT_0 && s_clone.load(Ordering::SeqCst) {
                        break;
                    }

                    let padding = match unsafe { dev.client.GetCurrentPadding() } {
                        Ok(p) => p,
                        Err(_) => {
                            // Dispositivo trocado (ex: plugou fone USB/Bluetooth) -> reabre automaticamente
                            dev_opt = None;
                            continue;
                        }
                    };

                    let avail_frames = dev.buffer_frames.saturating_sub(padding) as usize;
                    if avail_frames == 0 {
                        continue;
                    }

                    let dst_ptr = match unsafe { dev.render.GetBuffer(avail_frames as u32) } {
                        Ok(ptr) => ptr as *mut f32,
                        Err(_) => {
                            dev_opt = None;
                            continue;
                        }
                    };

                    let vol = if m_clone.load(Ordering::Relaxed) {
                        0.0
                    } else {
                        f32::from_bits(v_clone.load(Ordering::Relaxed)).clamp(0.0, 1.5)
                    };

                    let out_slice = unsafe {
                        std::slice::from_raw_parts_mut(dst_ptr, avail_frames * CHANNELS)
                    };

                    {
                        let mut q = r_clone.lock();
                        let queued_frames = q.len() / CHANNELS;

                        // Proteção contra teto extremo
                        if queued_frames > HARD_CAP_FRAMES {
                            let keep = (target_prebuffer + (SAMPLE_RATE as usize * 20 / 1000)) * CHANNELS;
                            let drop_samples = q.len().saturating_sub(keep);
                            let aligned_drop = drop_samples - (drop_samples % CHANNELS);
                            q.drain(..aligned_drop);
                        }

                        let queued_frames = q.len() / CHANNELS;

                        if buffering {
                            if queued_frames >= target_prebuffer {
                                buffering = false;
                                fade_pos = 0;
                            } else {
                                out_slice.fill(0.0);
                            }
                        }

                        if !buffering {
                            // Se a fila passar de target + 50ms, aplica catch-up suave (~1.02x) imperceptível
                            let high_water = target_prebuffer + (SAMPLE_RATE as usize * 50) / 1000;
                            let do_catchup = queued_frames > high_water;

                            for f in 0..avail_frames {
                                if q.len() < CHANNELS {
                                    // Underrun detectado: preenche resto com silêncio e reativa prebuffer adaptativo
                                    out_slice[f * CHANNELS..].fill(0.0);
                                    buffering = true;
                                    if last_underrun.elapsed() < Duration::from_secs(5) {
                                        target_prebuffer = (target_prebuffer + (SAMPLE_RATE as usize * 10 / 1000))
                                            .min(MAX_PREBUFFER_FRAMES);
                                    }
                                    last_underrun = Instant::now();
                                    break;
                                }

                                let mut l = q.pop_front().unwrap_or(0.0);
                                let mut r = q.pop_front().unwrap_or(0.0);

                                // Catch-up suave por interpolação linear a cada 48 frames (~2% mais rápido sem estalo)
                                if do_catchup {
                                    drift_step += 1;
                                    if drift_step >= 48 && q.len() >= CHANNELS {
                                        drift_step = 0;
                                        let nl = q.pop_front().unwrap_or(l);
                                        let nr = q.pop_front().unwrap_or(r);
                                        l = 0.5 * (l + nl);
                                        r = 0.5 * (r + nr);
                                    }
                                } else if last_underrun.elapsed() > Duration::from_secs(8)
                                    && target_prebuffer > MIN_PREBUFFER_FRAMES
                                {
                                    target_prebuffer = target_prebuffer
                                        .saturating_sub(1)
                                        .max(MIN_PREBUFFER_FRAMES);
                                }

                                let fade_gain = if fade_pos < FADE_IN_FRAMES {
                                    let g = fade_pos as f32 / FADE_IN_FRAMES as f32;
                                    fade_pos += 1;
                                    g
                                } else {
                                    1.0
                                };

                                let g = vol * fade_gain;
                                out_slice[f * CHANNELS] = (l * g).clamp(-1.0, 1.0);
                                out_slice[f * CHANNELS + 1] = (r * g).clamp(-1.0, 1.0);
                            }
                        }
                    }

                    let _ = unsafe { dev.render.ReleaseBuffer(avail_frames as u32, 0) };
                }

                drop(dev_opt);
                unsafe {
                    let _ = windows::Win32::Media::timeEndPeriod(1);
                }
            })
            .ok();

        Self {
            ring,
            volume_bits,
            muted,
            stop,
            thread,
        }
    }

    /// Empilha amostras PCM estéreo intercaladas (`[L, R, L, R, ...]`) em 48 kHz.
    pub fn push_samples(&self, pcm_interleaved: &[f32]) {
        if pcm_interleaved.is_empty() {
            return;
        }
        let mut q = self.ring.lock();
        q.extend(pcm_interleaved.iter().copied());
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume_bits
            .store(volume.clamp(0.0, 1.5).to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }
}

impl Drop for NativeAudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        ACTIVE_PLAYERS.fetch_sub(1, Ordering::SeqCst);
    }
}
