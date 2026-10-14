//! Captura de áudio com isolamento do Discord.
//!
//! `AudioCapturer` escolhe e mantém a estratégia de captura (`CapturePlan`) em tempo real:
//! reavalia a tabela de processos a cada 250 ms e faz *hot-swap* sem lacunas se o Discord abrir,
//! fechar ou reiniciar. Um relógio de 20 ms entrega quadros PCM f32 estéreo 48 kHz.

pub mod analysis;
pub mod discord_filter;
pub mod opus_enc;
pub mod process_tree;

#[cfg(windows)]
pub mod loopback;

#[cfg(windows)]
pub use capturer::*;

#[cfg(windows)]
mod capturer {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use parking_lot::Mutex;

    use super::discord_filter::{normalize, plan_system, plan_window, CapturePlan};
    use super::loopback::*;
    use super::process_tree::snapshot;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AudioTarget {
        /// Tela inteira: áudio do sistema, exceto o Discord.
        System,
        /// Aplicativo específico: somente a árvore do processo dono da janela.
        Window { pid: u32 },
    }

    struct Shared {
        sources: Mutex<Vec<LoopbackCapture>>,
        plan: Mutex<CapturePlan>,
        muted: AtomicBool,
        stop: AtomicBool,
    }

    pub struct AudioCapturer {
        shared: Arc<Shared>,
        threads: Vec<JoinHandle<()>>,
    }

    fn compute_plan(target: AudioTarget, names: &[String]) -> CapturePlan {
        let table = snapshot();
        match target {
            AudioTarget::System => plan_system(&table, names, std::process::id(), active_session_pids),
            AudioTarget::Window { pid } => plan_window(&table, names, pid),
        }
    }

    fn build_sources(plan: &CapturePlan) -> Result<Vec<LoopbackCapture>> {
        Ok(match plan {
            CapturePlan::Blocked => vec![],
            CapturePlan::Exclude { root } => vec![LoopbackCapture::start(LoopbackMode::ExcludeTree(*root))?],
            CapturePlan::Include { roots } => roots
                .iter()
                .filter_map(|r| match LoopbackCapture::start(LoopbackMode::IncludeTree(*r)) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!("loopback include pid {r} falhou: {e:#}");
                        None
                    }
                })
                .collect(),
        })
    }

    impl AudioCapturer {
        /// Inicia a captura. `on_frame` recebe 960 amostras estéreo (1920 f32) a cada 20 ms,
        /// exceto quando mudo ou bloqueado.
        pub fn start(
            target: AudioTarget,
            excluded: &[String],
            mut on_frame: Box<dyn FnMut(&[f32]) + Send>,
        ) -> Result<Self> {
            let names = normalize(excluded);
            let plan = compute_plan(target, &names);
            tracing::info!("áudio: plano inicial {}", plan.describe());
            let sources = build_sources(&plan)?;
            let shared = Arc::new(Shared {
                sources: Mutex::new(sources),
                plan: Mutex::new(plan),
                muted: AtomicBool::new(false),
                stop: AtomicBool::new(false),
            });

            // Planejador: reavalia a política a cada 250 ms e troca as fontes sem lacunas.
            let sh = shared.clone();
            let planner = std::thread::Builder::new().name("audio-planner".into()).spawn(move || {
                let _com = ComGuard::mta();
                while !sh.stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                    let new_plan = compute_plan(target, &names);
                    if new_plan == *sh.plan.lock() {
                        continue;
                    }
                    tracing::info!("áudio: hot-swap → {}", new_plan.describe());
                    match build_sources(&new_plan) {
                        Ok(new_sources) => {
                            // nova captura já está rodando: troca atômica; a antiga é descartada (drop → join)
                            let old = std::mem::replace(&mut *sh.sources.lock(), new_sources);
                            *sh.plan.lock() = new_plan;
                            drop(old);
                        }
                        Err(e) => tracing::warn!("áudio: hot-swap falhou: {e:#}"),
                    }
                }
            })?;

            // Relógio de 20 ms: mixa os anéis das fontes e entrega o quadro.
            let sh = shared.clone();
            let pump = std::thread::Builder::new().name("audio-pump".into()).spawn(move || {
                unsafe { windows::Win32::Media::timeBeginPeriod(1) };
                let step = Duration::from_millis(20);
                let mut next = Instant::now() + step;
                let mut mix = vec![0f32; FRAME_SAMPLES * CHANNELS];
                let mut tmp = vec![0f32; FRAME_SAMPLES * CHANNELS];
                while !sh.stop.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    }
                    next += step;
                    if Instant::now() > next + step * 5 {
                        next = Instant::now() + step; // atrasou demais: ressincroniza
                    }
                    mix.fill(0.0);
                    let blocked = *sh.plan.lock() == CapturePlan::Blocked;
                    {
                        let srcs = sh.sources.lock();
                        for s in srcs.iter() {
                            s.pop_frame(&mut tmp);
                            for (m, t) in mix.iter_mut().zip(tmp.iter()) {
                                *m += *t;
                            }
                        }
                    }
                    if sh.muted.load(Ordering::Relaxed) || blocked {
                        continue;
                    }
                    for m in mix.iter_mut() {
                        *m = m.clamp(-1.0, 1.0);
                    }
                    on_frame(&mix);
                }
                unsafe { windows::Win32::Media::timeEndPeriod(1) };
            })?;

            Ok(Self { shared, threads: vec![planner, pump] })
        }

        pub fn set_muted(&self, muted: bool) {
            self.shared.muted.store(muted, Ordering::Relaxed);
        }

        pub fn is_blocked(&self) -> bool {
            *self.shared.plan.lock() == CapturePlan::Blocked
        }

        pub fn describe(&self) -> String {
            self.shared.plan.lock().describe()
        }
    }

    impl Drop for AudioCapturer {
        fn drop(&mut self) {
            self.shared.stop.store(true, Ordering::SeqCst);
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
            self.shared.sources.lock().clear();
        }
    }
}
