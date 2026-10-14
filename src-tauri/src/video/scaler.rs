//! Escala + conversão BGRA → NV12 inteiramente na GPU (D3D11 Video Processor).

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::d3d::Gpu;

const POOL: usize = 8;

pub struct Scaler {
    gpu: Gpu,
    vctx: ID3D11VideoContext,
    vdev: ID3D11VideoDevice,
    enumr: ID3D11VideoProcessorEnumerator,
    proc: ID3D11VideoProcessor,
    out_tex: Vec<ID3D11Texture2D>,
    out_views: Vec<ID3D11VideoProcessorOutputView>,
    next: usize,
    fps: u32,
    pub in_size: (u32, u32),
    pub out_size: (u32, u32),
}

unsafe impl Send for Scaler {}

impl Scaler {
    pub fn new(gpu: &Gpu, in_size: (u32, u32), out_size: (u32, u32), fps: u32) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast().context("ID3D11VideoDevice")?;
        let vctx: ID3D11VideoContext = gpu.context.cast().context("ID3D11VideoContext")?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            InputWidth: in_size.0,
            InputHeight: in_size.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            OutputWidth: out_size.0,
            OutputHeight: out_size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumr = unsafe { vdev.CreateVideoProcessorEnumerator(&desc)? };
        let proc = unsafe { vdev.CreateVideoProcessor(&enumr, 0)? };

        unsafe {
            // Entrada: RGB full range. Saída: YCbCr BT.709, faixa limitada (16-235).
            // bits: Usage:1 | RGB_Range:1 | YCbCr_Matrix:1 | xvYCC:1 | Nominal_Range:2
            let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            vctx.VideoProcessorSetStreamColorSpace(&proc, 0, &in_cs);
            vctx.VideoProcessorSetOutputColorSpace(&proc, &out_cs);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&proc, 0, false);
            let src = RECT { left: 0, top: 0, right: in_size.0 as i32, bottom: in_size.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: out_size.0 as i32, bottom: out_size.1 as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&proc, 0, true, Some(&src));
            vctx.VideoProcessorSetStreamDestRect(&proc, 0, true, Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&proc, true, Some(&dst));
        }

        let mut out_tex = Vec::new();
        let mut out_views = Vec::new();
        for _ in 0..POOL {
            let td = D3D11_TEXTURE2D_DESC {
                Width: out_size.0,
                Height: out_size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            unsafe { gpu.device.CreateTexture2D(&td, None, Some(&mut tex)).context("CreateTexture2D NV12")? };
            let tex = tex.context("textura NV12 nula")?;
            let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut view = None;
            unsafe { vdev.CreateVideoProcessorOutputView(&tex, &enumr, &vd, Some(&mut view)).context("OutputView")? };
            out_tex.push(tex);
            out_views.push(view.context("output view nula")?);
        }
        Ok(Self { gpu: gpu.clone(), vctx, vdev, enumr, proc, out_tex, out_views, next: 0, fps, in_size, out_size })
    }

    /// Adapta dinamicamente o processador para novas dimensões de entrada sem perdas nem corte.
    fn reconfigure_input(&mut self, new_in_size: (u32, u32)) -> Result<()> {
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: self.fps, Denominator: 1 },
            InputWidth: new_in_size.0,
            InputHeight: new_in_size.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: self.fps, Denominator: 1 },
            OutputWidth: self.out_size.0,
            OutputHeight: self.out_size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumr = unsafe { self.vdev.CreateVideoProcessorEnumerator(&desc)? };
        let proc = unsafe { self.vdev.CreateVideoProcessor(&enumr, 0)? };

        unsafe {
            let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            self.vctx.VideoProcessorSetStreamColorSpace(&proc, 0, &in_cs);
            self.vctx.VideoProcessorSetOutputColorSpace(&proc, &out_cs);
            self.vctx.VideoProcessorSetStreamAutoProcessingMode(&proc, 0, false);
            let src = RECT { left: 0, top: 0, right: new_in_size.0 as i32, bottom: new_in_size.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: self.out_size.0 as i32, bottom: self.out_size.1 as i32 };
            self.vctx.VideoProcessorSetStreamSourceRect(&proc, 0, true, Some(&src));
            self.vctx.VideoProcessorSetStreamDestRect(&proc, 0, true, Some(&dst));
            self.vctx.VideoProcessorSetOutputTargetRect(&proc, true, Some(&dst));
        }

        let mut new_out_views = Vec::new();
        for tex in &self.out_tex {
            let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut view = None;
            unsafe { self.vdev.CreateVideoProcessorOutputView(tex, &enumr, &vd, Some(&mut view)).context("OutputView")? };
            new_out_views.push(view.context("output view nula")?);
        }

        self.enumr = enumr;
        self.proc = proc;
        self.out_views = new_out_views;
        self.in_size = new_in_size;
        tracing::info!("scaler: adaptado dinamicamente para resolução de entrada {}x{}", new_in_size.0, new_in_size.1);
        Ok(())
    }

    /// Converte/escala `src` (BGRA, qualquer tamanho de entrada) para uma textura NV12 do pool.
    pub fn process(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        if (desc.Width != self.in_size.0 || desc.Height != self.in_size.1) && desc.Width > 0 && desc.Height > 0 {
            self.reconfigure_input((desc.Width, desc.Height))?;
        }

        let vd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut in_view = None;
        unsafe {
            self.vdev
                .CreateVideoProcessorInputView(src, &self.enumr, &vd, Some(&mut in_view))
                .context("CreateVideoProcessorInputView")?
        };
        let idx = self.next;
        self.next = (self.next + 1) % POOL;
        let mut stream: D3D11_VIDEO_PROCESSOR_STREAM = unsafe { std::mem::zeroed() };
        stream.Enable = true.into();
        stream.pInputSurface = std::mem::ManuallyDrop::new(in_view);
        let mut streams = [stream];
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.proc, &self.out_views[idx], 0, &streams) };
        // libera a referência extra da view de entrada
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r.context("VideoProcessorBlt")?;
        let _ = &self.gpu;
        Ok(self.out_tex[idx].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scaler_2k_to_1080p() -> Result<()> {
        let gpu = Gpu::new()?;
        let mut scaler = Scaler::new(&gpu, (2560, 1440), (1920, 1080), 60)?;

        let td = D3D11_TEXTURE2D_DESC {
            Width: 2560,
            Height: 1440,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut pixels = vec![0u8; 2560 * 1440 * 4];
        // Coloca branco (255, 255, 255, 255) no canto inferior direito (x >= 2000, y >= 1200)
        for y in 1200..1440 {
            for x in 2000..2560 {
                let idx = (y * 2560 + x) * 4;
                pixels[idx] = 255;
                pixels[idx + 1] = 255;
                pixels[idx + 2] = 255;
                pixels[idx + 3] = 255;
            }
        }

        let init_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: 2560 * 4,
            SysMemSlicePitch: 0,
        };

        let mut tex = None;
        unsafe { gpu.device.CreateTexture2D(&td, Some(&init_data), Some(&mut tex))? };
        let tex = tex.unwrap();

        let out = scaler.process(&tex)?;
        let mut out_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { out.GetDesc(&mut out_desc) };
        println!("Scaler out desc: {}x{}", out_desc.Width, out_desc.Height);

        // Readback out NV12
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: 1920,
            Height: 1080,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.unwrap();
        unsafe { gpu.context.CopyResource(&staging, &out) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
        
        // Checa brilho no canto inferior direito do output (x=1800, y=1000)
        let ptr = mapped.pData as *const u8;
        let pitch = mapped.RowPitch as usize;
        let mut corner_sum = 0u64;
        for y in 950..1050 {
            for x in 1600..1850 {
                corner_sum += unsafe { *ptr.add(y * pitch + x) } as u64;
            }
        }
        unsafe { gpu.context.Unmap(&staging, 0) };

        println!("Corner luminance sum (expected > 0 if SCALED): {}", corner_sum);
        assert!(corner_sum > 100_000, "Scaler CROPPED instead of scaling! corner_sum was {}", corner_sum);
        Ok(())
    }

    #[test]
    fn test_scaler_dynamic_adaptation() -> Result<()> {
        let gpu = Gpu::new()?;
        // Inicializa com 1080p -> 1080p
        let mut scaler = Scaler::new(&gpu, (1920, 1080), (1920, 1080), 60)?;

        // Agora envia uma textura 2K (2560x1440) com pixels brancos no canto inferior direito
        let td = D3D11_TEXTURE2D_DESC {
            Width: 2560,
            Height: 1440,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut pixels = vec![0u8; 2560 * 1440 * 4];
        for y in 1200..1440 {
            for x in 2000..2560 {
                let idx = (y * 2560 + x) * 4;
                pixels[idx] = 255;
                pixels[idx + 1] = 255;
                pixels[idx + 2] = 255;
                pixels[idx + 3] = 255;
            }
        }
        let init_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: 2560 * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex = None;
        unsafe { gpu.device.CreateTexture2D(&td, Some(&init_data), Some(&mut tex))? };
        let tex = tex.unwrap();

        // O scaler deve reconfigurar automaticamente para 2560x1440 e escalar todo o quadro para 1920x1080
        let out = scaler.process(&tex)?;
        assert_eq!(scaler.in_size, (2560, 1440));

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: 1920,
            Height: 1080,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.unwrap();
        unsafe { gpu.context.CopyResource(&staging, &out) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

        let ptr = mapped.pData as *const u8;
        let pitch = mapped.RowPitch as usize;
        let mut corner_sum = 0u64;
        for y in 950..1050 {
            for x in 1600..1850 {
                corner_sum += unsafe { *ptr.add(y * pitch + x) } as u64;
            }
        }
        unsafe { gpu.context.Unmap(&staging, 0) };

        println!("Dynamic adaptation corner sum: {}", corner_sum);
        assert!(corner_sum > 100_000, "Scaler CROPPED instead of adapting! corner_sum was {}", corner_sum);
        Ok(())
    }
}
