//! Dispositivo Direct3D 11 compartilhado entre captura (WGC), escala (VideoProcessor) e encoder (MF).

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

// Os objetos D3D11 são thread-safe quando `ID3D11Multithread::SetMultithreadProtected(true)`
// (habilitado em `Gpu::new`). Usamos o dispositivo a partir de várias threads de forma controlada.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    pub fn new() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
        }
        let device = device.context("device nulo")?;
        let context = context.context("context nulo")?;
        let mt: ID3D11Multithread = device.cast()?;
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok(Self { device, context })
    }

    pub fn adapter_name(&self) -> String {
        use windows::Win32::Graphics::Dxgi::IDXGIDevice;
        (|| -> Result<String> {
            let dxgi: IDXGIDevice = self.device.cast()?;
            let adapter = unsafe { dxgi.GetAdapter()? };
            let desc = unsafe { adapter.GetDesc()? };
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
            Ok(String::from_utf16_lossy(&desc.Description[..len]))
        })()
        .unwrap_or_else(|_| "GPU".into())
    }
}
