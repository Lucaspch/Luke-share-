//! Toca um tom senoidal via WASAPI (shared, render) por N segundos.
//! Uso: tone_player <freq_hz> <segundos>
//! Para testar a exclusão: copie este .exe como `Discord.exe` (ou outro nome da lista).

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

fn main() -> windows::core::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let freq: f32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(440.0);
    let secs: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10.0);
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;
        let channels = (*fmt).nChannels as usize;
        let rate = (*fmt).nSamplesPerSec as f32;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 10_000_000, 0, fmt, None)?;
        let buf_frames = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        client.Start()?;
        let total = (secs * rate) as u64;
        let mut written = 0u64;
        let mut phase = 0f32;
        let step = 2.0 * std::f32::consts::PI * freq / rate;
        while written < total {
            let pad = client.GetCurrentPadding()?;
            let free = buf_frames - pad;
            if free > 0 {
                let p = render.GetBuffer(free)? as *mut f32; // formato de mix do WASAPI é float32
                for i in 0..free as usize {
                    let v = 0.1 * phase.sin();
                    phase += step;
                    if phase > 2.0 * std::f32::consts::PI {
                        phase -= 2.0 * std::f32::consts::PI;
                    }
                    for c in 0..channels {
                        *p.add(i * channels + c) = v;
                    }
                }
                render.ReleaseBuffer(free, 0)?;
                written += free as u64;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        client.Stop()?;
    }
    Ok(())
}
