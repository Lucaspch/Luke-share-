//! Sonda de vídeo: testa enumeração de fontes, WGC, D3D11 Scaler e MFT H.264.
//!
//! Uso:
//!   video_probe --list
//!   video_probe --seconds 2 [--preset 1080|720|480|source] [--out test.h264]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use luke_share_lib::capture::sources::list_sources;
use luke_share_lib::capture::wgc::WgcCapture;
use luke_share_lib::video::d3d::Gpu;
use luke_share_lib::video::encoder::{EncoderConfig, H264Encoder};
use luke_share_lib::video::h264::stream_size;
use luke_share_lib::video::resolution::{target_size, Preset};
use luke_share_lib::video::scaler::Scaler;

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1).cloned())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let gpu = Gpu::new()?;
    println!("GPU: {}", gpu.adapter_name());

    let sources = list_sources();
    if args.iter().any(|a| a == "--list") || args.len() <= 1 {
        println!("Fontes disponíveis ({}):", sources.len());
        for s in &sources {
            match s {
                luke_share_lib::capture::sources::SourceInfo::Monitor { id, name, width, height, primary, refresh_rate } => {
                    println!("  [Monitor id={id}] {name} ({}x{} @ {refresh_rate}Hz) primary={primary}", width, height);
                }
                luke_share_lib::capture::sources::SourceInfo::Window { id, title, pid, exe, refresh_rate } => {
                    println!("  [Janela id={id} pid={pid} exe={exe} @ {refresh_rate}Hz] \"{title}\"");
                }
            }
        }
        if args.len() <= 1 {
            println!("\nDica: execute com --seconds 2 para testar a captura + encode.");
            return Ok(());
        }
    }

    let secs: f32 = arg(&args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(2.0);
    let preset = match arg(&args, "--preset").as_deref() {
        Some("720") => Preset::P720,
        Some("480") => Preset::P480,
        Some("source") => Preset::Source,
        _ => Preset::P1080,
    };

    let target_id: Option<isize> = arg(&args, "--id").and_then(|s| s.parse().ok());
    let Some(src) = (if let Some(id) = target_id {
        sources.iter().find(|s| s.id() == id)
    } else {
        sources
            .iter()
            .find(|s| match s {
                luke_share_lib::capture::sources::SourceInfo::Monitor { primary, .. } => *primary,
                _ => false,
            })
            .or_else(|| sources.first())
    }) else {
        println!("Nenhuma fonte encontrada.");
        return Ok(());
    };
    println!("Capturando fonte: {:?}", src);

    let (frame_tx, frame_rx) = crossbeam_channel::bounded(4);
    let cap = WgcCapture::start(&gpu, src, 60, frame_tx)?;
    println!("Captura iniciada: tamanho nativo {}x{}", cap.initial_size.0, cap.initial_size.1);

    let (out_w, out_h) = target_size(cap.initial_size.0, cap.initial_size.1, preset);
    println!("Resolução alvo: {}x{} ({:?})", out_w, out_h, preset);

    let mut scaler = Scaler::new(&gpu, cap.initial_size, (out_w, out_h), 60)?;

    let enc_cfg = EncoderConfig {
        width: out_w,
        height: out_h,
        fps: 60,
        bitrate: preset.default_bitrate(),
    };

    let frame_count = Arc::new(AtomicUsize::new(0));
    let key_count = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicUsize::new(0));
    let first_dim = Arc::new(parking_lot::Mutex::new(None));

    let f_cnt = frame_count.clone();
    let k_cnt = key_count.clone();
    let b_cnt = total_bytes.clone();
    let dim = first_dim.clone();

    let out_file = arg(&args, "--out");
    let mut out_h264 = out_file.map(std::fs::File::create).transpose()?;

    let encoder = H264Encoder::start(
        &gpu,
        enc_cfg,
        true,
        Box::new(move |ef| {
            f_cnt.fetch_add(1, Ordering::SeqCst);
            if ef.key {
                k_cnt.fetch_add(1, Ordering::SeqCst);
            }
            b_cnt.fetch_add(ef.data.len(), Ordering::SeqCst);
            if dim.lock().is_none() {
                if let Some(d) = stream_size(&ef.data) {
                    *dim.lock() = Some(d);
                }
            }
            if let Some(ref mut f) = out_h264 {
                use std::io::Write;
                let _ = f.write_all(&ef.data);
            }
        }),
    )?;

    println!("Encoder selecionado: {} (hardware={})", encoder.name, encoder.hardware);

    let start = std::time::Instant::now();
    let mut rx_count = 0;
    while start.elapsed() < Duration::from_secs_f32(secs) {
        match frame_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                rx_count += 1;
                match scaler.process(&frame.texture) {
                    Ok(nv12) => {
                        let sub = encoder.submit(nv12, frame.pts_100ns);
                        if rx_count <= 3 {
                            eprintln!("[VP] Frame {} recebido e submetido (submit={sub})", rx_count);
                        }
                    }
                    Err(e) => eprintln!("[VP] Erro no scaler: {e}"),
                }
            }
            Err(_) => {}
        }
    }
    eprintln!("[VP] Total de frames saídos do WGC: {rx_count}");

    drop(cap);
    drop(encoder);

    let total_frames = frame_count.load(Ordering::SeqCst);
    let total_keys = key_count.load(Ordering::SeqCst);
    let bytes = total_bytes.load(Ordering::SeqCst);

    println!("\n=== Resultado da captura de vídeo ===");
    println!("Frames codificados: {}", total_frames);
    println!("Keyframes (IDR):    {}", total_keys);
    println!("Total de bytes:     {} ({:.2} KB)", bytes, bytes as f64 / 1024.0);
    if let Some((w, h)) = *first_dim.lock() {
        println!("Dimensões no SPS:   {}x{}", w, h);
    }
    println!("FPS médio:          {:.1}", total_frames as f64 / secs as f64);

    if total_frames > 0 && total_keys > 0 {
        println!("\n[PASS] Pipeline de vídeo validado com sucesso!");
    } else {
        println!("\n[FAIL] Nenhum frame foi codificado.");
    }

    Ok(())
}
