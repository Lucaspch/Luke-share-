//! Sonda de áudio: captura via `AudioCapturer` (mesma lógica do app) e mede tons por Goertzel.
//!
//! Uso:
//!   audio_probe --mode system|window [--pid N] [--seconds 5] [--names discord.exe,...]
//!               [--freqs 440,880] [--wav out.wav]
//! Imprime a amplitude (dB) de cada frequência e o plano de captura usado.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use luke_share_lib::audio::analysis::*;
use luke_share_lib::audio::discord_filter::default_excluded;
use luke_share_lib::audio::{AudioCapturer, AudioTarget};

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1).cloned())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = arg(&args, "--mode").unwrap_or_else(|| "system".into());
    let secs: f32 = arg(&args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let names: Vec<String> = arg(&args, "--names")
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_else(default_excluded);
    let freqs: Vec<f32> = arg(&args, "--freqs")
        .map(|s| s.split(',').filter_map(|f| f.parse().ok()).collect())
        .unwrap_or_else(|| vec![440.0, 880.0]);
    let target = match mode.as_str() {
        "window" => AudioTarget::Window { pid: arg(&args, "--pid").and_then(|s| s.parse().ok()).expect("--pid obrigatório") },
        _ => AudioTarget::System,
    };

    let store: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = store.clone();
    let cap = AudioCapturer::start(target, &names, Box::new(move |f| s2.lock().unwrap().extend_from_slice(f)))?;
    println!("plano: {}", cap.describe());
    std::thread::sleep(Duration::from_secs_f32(secs));
    println!("plano final: {} (bloqueado={})", cap.describe(), cap.is_blocked());
    drop(cap);

    let samples = store.lock().unwrap().clone();
    let mono = to_mono(&samples, 2);
    // ignora o início (partida / hot-swap); configurável com --skip <segundos>
    let skip_s: f32 = arg(&args, "--skip").and_then(|s| s.parse().ok()).unwrap_or(0.5);
    let skip = (48_000.0 * skip_s) as usize;
    let body = if mono.len() > skip { &mono[skip..] } else { &mono[..] };
    println!("amostras: {} ({:.2}s)", mono.len(), mono.len() as f32 / 48000.0);
    for f in &freqs {
        println!("FREQ {f:.0} Hz: {:.1} dB", db(goertzel_amp(body, 48000.0, *f)));
    }
    if let Some(p) = arg(&args, "--wav") {
        write_wav_f32(std::path::Path::new(&p), &samples, 2, 48000)?;
    }
    Ok(())
}
