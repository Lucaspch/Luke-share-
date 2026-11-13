//! Enumeração e captura nativa de Webcam no Windows via Media Foundation (`IMFSourceReader`).
//! Entrega frames diretamente como `ID3D11Texture2D` (BGRA) para reaproveitar todo o pipeline GPU (`Scaler` -> `H264Encoder`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

use crate::capture::wgc::Frame;
use crate::video::d3d::Gpu;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CameraDeviceInfo {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

fn ensure_mf_started() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_LITE).context("MFStartup falhou")?;
    }
    Ok(())
}

fn read_activate_string(act: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
    unsafe {
        let mut ptr = windows::core::PWSTR::null();
        let mut len = 0u32;
        if act.GetAllocatedString(key, &mut ptr, &mut len).is_ok() {
            let s = ptr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(ptr.0 as *const _));
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn unpack_u64_pair(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, (v & 0xFFFF_FFFF) as u32)
}

fn probe_camera_native_mode(source: &IMFMediaSource) -> (u32, u32, u32) {
    unsafe {
        let Ok(pd) = source.CreatePresentationDescriptor() else {
            return (1280, 720, 30);
        };
        let mut selected = windows::core::BOOL(0);
        let mut sd_opt = None;
        if pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd_opt).is_err() {
            return (1280, 720, 30);
        }
        let Some(sd) = sd_opt else {
            return (1280, 720, 30);
        };
        let Ok(handler) = sd.GetMediaTypeHandler() else {
            return (1280, 720, 30);
        };
        let count = handler.GetMediaTypeCount().unwrap_or(0);
        let mut best = (0u32, 0u32, 0u32, 0i64);

        for i in 0..count {
            let Ok(mt) = handler.GetMediaTypeByIndex(i) else { continue };
            let Ok(size_u64) = mt.GetUINT64(&MF_MT_FRAME_SIZE) else { continue };
            let (w, h) = unpack_u64_pair(size_u64);
            if w == 0 || h == 0 || w > 1920 || h > 1080 {
                continue;
            }
            let fps = mt
                .GetUINT64(&MF_MT_FRAME_RATE)
                .map(|r| {
                    let (num, den) = unpack_u64_pair(r);
                    if den > 0 { (num / den).clamp(15, 60) } else { 30 }
                })
                .unwrap_or(30);

            // Pontua preferindo 720p/1080p @ 30fps+ para baixa latência em webcam
            let res_score = if w == 1280 && h == 720 {
                500_000i64
            } else if w == 1920 && h == 1080 {
                480_000i64
            } else {
                (w * h) as i64 / 4
            };
            let score = res_score + (fps as i64) * 1000;
            if score > best.3 {
                best = (w, h, fps, score);
            }
        }

        if best.0 >= 320 && best.1 >= 240 {
            (best.0, best.1, best.2.max(24))
        } else {
            (1280, 720, 30)
        }
    }
}

/// Lista webcams conectadas ao Windows sem entrar em pânico caso não existam dispositivos.
pub fn list_cameras() -> Vec<CameraDeviceInfo> {
    if ensure_mf_started().is_err() {
        return Vec::new();
    }

    let mut out = Vec::new();
    unsafe {
        let mut attrs = None;
        if MFCreateAttributes(&mut attrs, 1).is_err() {
            return out;
        }
        let Some(attrs) = attrs else { return out };
        if attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .is_err()
        {
            return out;
        }

        let mut raw_list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFEnumDeviceSources(&attrs, &mut raw_list, &mut count).is_err() || raw_list.is_null() {
            return out;
        }

        let slice = std::slice::from_raw_parts(raw_list, count as usize);
        for (idx, item) in slice.iter().enumerate() {
            if let Some(act) = item {
                let name = read_activate_string(act, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                    .unwrap_or_else(|| format!("Webcam {}", idx + 1));
                let sym = read_activate_string(act, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
                    .unwrap_or_else(|| format!("cam-{idx}"));

                let (w, h, fps) = match act.ActivateObject::<IMFMediaSource>() {
                    Ok(src) => {
                        let res = probe_camera_native_mode(&src);
                        let _ = src.Shutdown();
                        res
                    }
                    Err(_) => (1280, 720, 30),
                };

                out.push(CameraDeviceInfo {
                    id: sym,
                    name,
                    width: w,
                    height: h,
                    fps,
                });
            }
        }
        CoTaskMemFree(Some(raw_list as *const _));
    }

    out
}

pub struct CameraCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub initial_size: (u32, u32),
}

unsafe impl Send for CameraCapture {}

const CAM_RING_LEN: usize = 4;

fn create_bgra_ring(gpu: &Gpu, w: u32, h: u32) -> Result<Vec<ID3D11Texture2D>> {
    let mut ring = Vec::with_capacity(CAM_RING_LEN);
    for _ in 0..CAM_RING_LEN {
        let td = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { gpu.device.CreateTexture2D(&td, None, Some(&mut tex))? };
        ring.push(tex.context("textura BGRA webcam nula")?);
    }
    Ok(ring)
}

fn open_camera_source(device_id: &str) -> Result<IMFMediaSource> {
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.context("MFCreateAttributes nulo")?;
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        attrs.SetString(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            windows::core::PCWSTR(wide.as_ptr()),
        )?;
        MFCreateDeviceSource(&attrs).context("Falha ao abrir dispositivo de câmera")
    }
}

impl CameraCapture {
    pub fn start(
        gpu: &Gpu,
        cam: &CameraDeviceInfo,
        fps: u32,
        tx: Sender<Frame>,
    ) -> Result<Self> {
        ensure_mf_started()?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = stop.clone();
        let gpu_bg = gpu.clone();
        let cam_clone = cam.clone();
        let target_fps = fps.max(15).min(60);

        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<(u32, u32)>>(1);

        let thread = std::thread::Builder::new()
            .name("camera-capture".into())
            .spawn(move || {
                if let Err(e) = ensure_mf_started() {
                    let _ = init_tx.send(Err(e));
                    return;
                }

                let source = match open_camera_source(&cam_clone.id) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };

                let reader_res: Result<(IMFSourceReader, u32, u32)> = (|| unsafe {
                    let mut r_attrs = None;
                    MFCreateAttributes(&mut r_attrs, 2)?;
                    let r_attrs = r_attrs.context("reader attrs nulo")?;
                    r_attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;

                    let reader = MFCreateSourceReaderFromMediaSource(&source, Some(&r_attrs))
                        .context("MFCreateSourceReaderFromMediaSource")?;

                    let out_mt = MFCreateMediaType()?;
                    out_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                    out_mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
                    let packed_size = ((cam_clone.width as u64) << 32) | (cam_clone.height as u64);
                    let _ = out_mt.SetUINT64(&MF_MT_FRAME_SIZE, packed_size);
                    let packed_fps = ((target_fps as u64) << 32) | 1u64;
                    let _ = out_mt.SetUINT64(&MF_MT_FRAME_RATE, packed_fps);

                    reader
                        .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &out_mt)
                        .context("SetCurrentMediaType RGB32")?;

                    let cur_mt = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?;
                    let actual_sz = cur_mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(packed_size);
                    let (w, h) = unpack_u64_pair(actual_sz);
                    Ok((reader, w.max(2), h.max(2)))
                })();

                let (reader, w, h) = match reader_res {
                    Ok(v) => v,
                    Err(e) => {
                        unsafe { let _ = source.Shutdown(); }
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };

                let mut ring = match create_bgra_ring(&gpu_bg, w, h) {
                    Ok(r) => r,
                    Err(e) => {
                        unsafe { let _ = source.Shutdown(); }
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };

                let _ = init_tx.send(Ok((w, h)));

                let mut ring_idx = 0usize;
                let mut cur_size = (w, h);
                let mut scratch_bgra: Vec<u8> = vec![0u8; (w * h * 4) as usize];
                let t0 = Instant::now();

                while !stop_bg.load(Ordering::SeqCst) {
                    unsafe {
                        let mut stream_idx = 0u32;
                        let mut flags = 0u32;
                        let mut timestamp = 0i64;
                        let mut sample: Option<IMFSample> = None;

                        let hr = reader.ReadSample(
                            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                            0,
                            Some(&mut stream_idx),
                            Some(&mut flags),
                            Some(&mut timestamp),
                            Some(&mut sample),
                        );

                        if hr.is_err() {
                            std::thread::sleep(std::time::Duration::from_millis(15));
                            continue;
                        }

                        if (flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0 {
                            if let Ok(mt) = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) {
                                if let Ok(sz) = mt.GetUINT64(&MF_MT_FRAME_SIZE) {
                                    let (nw, nh) = unpack_u64_pair(sz);
                                    if nw >= 2 && nh >= 2 && (nw, nh) != cur_size {
                                        if let Ok(new_ring) = create_bgra_ring(&gpu_bg, nw, nh) {
                                            ring = new_ring;
                                            cur_size = (nw, nh);
                                            ring_idx = 0;
                                            scratch_bgra.resize((nw * nh * 4) as usize, 0);
                                        }
                                    }
                                }
                            }
                        }

                        let Some(sample) = sample else {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        };

                        let Ok(buf) = sample.ConvertToContiguousBuffer() else { continue };
                        let mut data_ptr: *mut u8 = std::ptr::null_mut();
                        let mut max_len = 0u32;
                        let mut cur_len = 0u32;
                        if buf
                            .Lock(&mut data_ptr, Some(&mut max_len), Some(&mut cur_len))
                            .is_err()
                        {
                            continue;
                        }

                        let (cw, ch) = cur_size;
                        let row_bytes = (cw * 4) as usize;
                        let needed = row_bytes * ch as usize;
                        if !data_ptr.is_null() && cur_len as usize >= needed {
                            let src_slice = std::slice::from_raw_parts(data_ptr, cur_len as usize);
                            let stride = cur_len as usize / ch as usize;
                            for y in 0..ch as usize {
                                let src_row = &src_slice[y * stride..y * stride + row_bytes];
                                let dst_row = &mut scratch_bgra[y * row_bytes..(y + 1) * row_bytes];
                                dst_row.copy_from_slice(src_row);
                                // Garante alpha opaco (255) caso o driver da câmera entregue XRGB com alpha = 0
                                for px in dst_row.chunks_exact_mut(4) {
                                    px[3] = 255;
                                }
                            }

                            let tex = ring[ring_idx].clone();
                            ring_idx = (ring_idx + 1) % ring.len();
                            gpu_bg.context.UpdateSubresource(
                                &tex,
                                0,
                                None,
                                scratch_bgra.as_ptr() as *const _,
                                row_bytes as u32,
                                needed as u32,
                            );

                            let pts = if timestamp > 0 {
                                timestamp
                            } else {
                                (t0.elapsed().as_nanos() / 100) as i64
                            };
                            let _ = tx.try_send(Frame {
                                texture: tex,
                                width: cw,
                                height: ch,
                                pts_100ns: pts,
                            });
                        }

                        let _ = buf.Unlock();
                    }
                }

                unsafe {
                    let _ = source.Shutdown();
                }
            })?;

        let initial_size = init_rx
            .recv()
            .context("thread de câmera abortada antes da inicialização")??;

        Ok(Self {
            stop,
            thread: Some(thread),
            initial_size,
        })
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_cameras_does_not_panic_without_hardware() {
        let cams = list_cameras();
        // Pode estar vazio em ambientes headless/CI, apenas garante que não entra em pânico
        for c in cams {
            assert!(!c.id.is_empty());
            assert!(c.width >= 2 && c.height >= 2);
        }
    }
}
