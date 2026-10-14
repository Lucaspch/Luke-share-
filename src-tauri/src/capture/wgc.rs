//! Windows Graphics Capture (WGC): captura de monitor (HMONITOR) ou janela (HWND).
//! Cada frame é copiado (GPU→GPU) para uma textura própria de um anel e enviado por canal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use super::sources::SourceInfo;
use crate::video::d3d::Gpu;

pub struct Frame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// Tempo do sistema em unidades de 100 ns.
    pub pts_100ns: i64,
}

struct Ring {
    size: (u32, u32),
    tex: Vec<ID3D11Texture2D>,
    idx: usize,
}

struct CbState {
    ring: Option<Ring>,
    last_size: SizeInt32,
    last_pts: i64,
}

// Texturas D3D11 são usadas sob `SetMultithreadProtected(true)`.
unsafe impl Send for CbState {}
unsafe impl Send for Frame {}

/// Envoltório para mover objetos COM thread-safe (agile) para o callback do WinRT.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}
impl<T> SendBox<T> {
    fn get(&self) -> &T {
        &self.0
    }
}

pub struct WgcCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    token: i64,
    closed_token: i64,
    pub closed: Arc<AtomicBool>,
    pub initial_size: (u32, u32),
}

unsafe impl Send for WgcCapture {}

const RING_LEN: usize = 5;

fn make_ring(gpu: &Gpu, w: u32, h: u32) -> Result<Ring> {
    let mut tex = Vec::new();
    for _ in 0..RING_LEN {
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
        let mut t = None;
        unsafe { gpu.device.CreateTexture2D(&td, None, Some(&mut t))? };
        tex.push(t.context("textura BGRA nula")?);
    }
    Ok(Ring { size: (w, h), tex, idx: 0 })
}

impl WgcCapture {
    /// Inicia a captura. Os frames são enviados em `tx` (descartados se o consumidor estiver lento).
    pub fn start(gpu: &Gpu, src: &SourceInfo, fps: u32, tx: Sender<Frame>) -> Result<Self> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(anyhow!("Windows Graphics Capture não é suportado neste sistema"));
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe {
            match src {
                SourceInfo::Monitor { id, .. } => interop.CreateForMonitor(HMONITOR(*id as *mut _))?,
                SourceInfo::Window { id, .. } => interop.CreateForWindow(HWND(*id as *mut _))?,
            }
        };
        let size = item.Size()?;
        let initial_size = match src {
            SourceInfo::Monitor { width, height, .. } => (*width, *height),
            SourceInfo::Window { .. } => (size.Width.max(2) as u32, size.Height.max(2) as u32),
        };
        let pool_size = SizeInt32 {
            Width: initial_size.0 as i32,
            Height: initial_size.1 as i32,
        };
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let d3d_dev: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d_dev, format, 3, pool_size)?;

        let min_interval = if fps == 0 {
            0
        } else {
            10_000_000i64 / fps.max(1) as i64
        };
        let state = Arc::new(Mutex::new(CbState { ring: None, last_size: pool_size, last_pts: i64::MIN / 2 }));
        let gpu2 = gpu.clone();
        let dev2 = SendBox(d3d_dev.clone());
        let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |sender, _| {
            let Some(pool) = sender.as_ref() else { return Ok(()) };
            let frame = pool.TryGetNextFrame()?;
            let content = frame.ContentSize()?;
            let pts = frame.SystemRelativeTime()?.Duration;
            let mut st = state.lock();
            if content.Width != st.last_size.Width || content.Height != st.last_size.Height {
                st.last_size = content;
                let _ = pool.Recreate(dev2.get(), format, 3, content);
            }
            // limitador de FPS (tolerância de 10%, desativado se fps == 0 destravado)
            if min_interval > 0 && pts - st.last_pts < min_interval * 9 / 10 {
                return Ok(());
            }
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            let w = (content.Width.max(2) as u32).min(desc.Width);
            let h = (content.Height.max(2) as u32).min(desc.Height);
            if st.ring.as_ref().map(|r| r.size) != Some((w, h)) {
                st.ring = Some(make_ring(&gpu2, w, h).map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?);
            }
            let ring = st.ring.as_mut().unwrap();
            let dst = ring.tex[ring.idx].clone();
            ring.idx = (ring.idx + 1) % RING_LEN;
            let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            unsafe { gpu2.context.CopySubresourceRegion(&dst, 0, 0, 0, 0, &tex, 0, Some(&bx)) };
            st.last_pts = pts;
            drop(st);
            // se o consumidor está lento, descarta (prioridade: baixa latência)
            let _ = tx.try_send(Frame { texture: dst, width: w, height: h, pts_100ns: pts });
            Ok(())
        });
        let token = pool.FrameArrived(&handler)?;

        let closed = Arc::new(AtomicBool::new(false));
        let c2 = closed.clone();
        let closed_token = item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            c2.store(true, Ordering::SeqCst);
            Ok(())
        }))?;

        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        let _ = session.SetIsBorderRequired(false); // Windows 11; ignorado onde não suportado
        session.StartCapture()?;
        Ok(Self {
            pool,
            session,
            item,
            token,
            closed_token,
            closed,
            initial_size,
        })
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
