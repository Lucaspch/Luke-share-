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
        Ok(Self { gpu: gpu.clone(), vctx, vdev, enumr, proc, out_tex, out_views, next: 0, in_size, out_size })
    }

    /// Converte/escala `src` (BGRA, tamanho = `in_size`) para uma textura NV12 do pool.
    pub fn process(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
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
