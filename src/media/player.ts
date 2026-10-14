//! Player de mídia de ultra-baixa latência usando WebCodecs (VideoDecoder) e Web Audio API (PCM f32).

export interface PlayerStats {
  width: number;
  height: number;
  fps: number;
}

export type StreamEvent =
  | { t: "video"; k: boolean; p: number; d: string }
  | { t: "audio"; d: string }
  | { t: "config"; width: number; height: number; fps: number }
  | { t: "audio_state"; enabled: boolean }
  | {
      t: "telemetry";
      rtt_ms: number;
      packets_rx: number;
      loss_pct: number;
      nacks_recovered: number;
      dropped_ref: number;
      fps: number;
    }
  | { t: "ended"; reason: string };

export class StreamPlayer {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private videoDecoder: VideoDecoder | null = null;
  private audioCtx: AudioContext | null = null;
  private gainNode: GainNode | null = null;
  private nextAudioPlayTime = 0;
  private overlay: HTMLElement | null;
  private statsCallback?: (stats: PlayerStats) => void;
  private onRequestKey?: () => void;

  private currentWidth = 0;
  private currentHeight = 0;
  private frameCount = 0;
  private lastFpsCalcTime = performance.now();
  private currentFps = 0;
  private hasFirstKeyFrame = false;

  constructor(
    canvas: HTMLCanvasElement,
    overlay: HTMLElement | null,
    statsCallback?: (stats: PlayerStats) => void,
    enableAudio = true,
    onRequestKey?: () => void
  ) {
    this.canvas = canvas;
    this.overlay = overlay;
    this.statsCallback = statsCallback;
    this.onRequestKey = onRequestKey;
    this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true })!;

    if (enableAudio) {
      this.initAudio();
    }
    this.initVideo();
  }

  private initAudio() {
    try {
      const AudioContextClass = window.AudioContext || (window as any).webkitAudioContext;
      this.audioCtx = new AudioContextClass({ sampleRate: 48000, latencyHint: "interactive" });
      this.gainNode = this.audioCtx.createGain();
      this.gainNode.connect(this.audioCtx.destination);
    } catch (e) {
      console.warn("Falha ao inicializar Web Audio Context:", e);
    }
  }

  private initVideo() {
    if (!("VideoDecoder" in window)) {
      console.error("WebCodecs VideoDecoder não suportado neste navegador.");
      return;
    }

    if (this.videoDecoder && this.videoDecoder.state !== "closed") {
      try {
        this.videoDecoder.close();
      } catch (_) {}
    }

    this.hasFirstKeyFrame = false;
    this.videoDecoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        if (this.overlay && !this.overlay.classList.contains("hidden")) {
          this.overlay.classList.add("hidden");
        }

        const w = frame.displayWidth;
        const h = frame.displayHeight;
        if (this.canvas.width !== w || this.canvas.height !== h) {
          this.canvas.width = w;
          this.canvas.height = h;
          this.currentWidth = w;
          this.currentHeight = h;
        }

        this.ctx.drawImage(frame, 0, 0, w, h);
        frame.close();

        this.frameCount++;
        const now = performance.now();
        if (now - this.lastFpsCalcTime >= 1000) {
          this.currentFps = Math.round((this.frameCount * 1000) / (now - this.lastFpsCalcTime));
          this.frameCount = 0;
          this.lastFpsCalcTime = now;
          if (this.statsCallback) {
            this.statsCallback({
              width: this.currentWidth,
              height: this.currentHeight,
              fps: this.currentFps,
            });
          }
        }
      },
      error: (e) => {
        console.warn("VideoDecoder reportou erro, reinicializando decodificador:", e);
        this.initVideo();
      },
    });

    this.configureDecoder();
  }

  private configureDecoder() {
    if (this.videoDecoder) {
      try {
        this.videoDecoder.configure({
          codec: "avc1.640034", // H.264 High Profile Level 5.2 (suporta até 240 FPS em 1080p)
          optimizeForLatency: true,
          hardwareAcceleration: "prefer-hardware",
        });
      } catch (_) {
        try {
          this.videoDecoder.configure({
            codec: "avc1.64002a", // Fallback Level 4.2
            optimizeForLatency: true,
            hardwareAcceleration: "prefer-hardware",
          });
        } catch (e) {
          console.warn("Erro ao configurar VideoDecoder:", e);
        }
      }
    }
  }

  public handleVideoFrame(base64Data: string, isKey: boolean, pts: number) {
    // Se o decoder fechou ou não foi inicializado, reinicia
    if (!this.videoDecoder || this.videoDecoder.state !== "configured") {
      this.initVideo();
      if (!this.videoDecoder || this.videoDecoder.state !== "configured") return;
    }

    // Se o decoder acumular mais de 8 frames na fila (lag severo de renderização/GPU):
    // Limpa a fila inteira com reset(), descarta até o próximo IDR e dispara pedido imediato de PLI ao host.
    // NUNCA descarte um P-frame isolado, pois quebra toda a predição temporal H.264 subsequente!
    if (this.videoDecoder.decodeQueueSize > 8) {
      console.warn("VideoDecoder decodeQueueSize > 8; resetando fila e solicitando IDR imediato...");
      try {
        this.videoDecoder.reset();
        this.configureDecoder();
      } catch (_) {
        this.initVideo();
      }
      this.hasFirstKeyFrame = false;
      this.onRequestKey?.();
      return;
    }

    // WebCodecs exige um keyframe como primeiro chunk após configure/reset
    if (!this.hasFirstKeyFrame) {
      if (!isKey) {
        return; // Aguarda o primeiro IDR antes de aceitar frames delta
      }
      this.hasFirstKeyFrame = true;
    }

    try {
      const binary = atob(base64Data);
      const len = binary.length;
      const bytes = new Uint8Array(len);
      for (let i = 0; i < len; i++) {
        bytes[i] = binary.charCodeAt(i);
      }

      const chunk = new EncodedVideoChunk({
        type: isKey ? "key" : "delta",
        timestamp: pts,
        data: bytes,
      });

      this.videoDecoder.decode(chunk);
    } catch (e) {
      console.warn("Erro decodificando frame de vídeo:", e);
    }
  }

  public handleAudioChunk(base64Data: string) {
    if (!this.audioCtx || !this.gainNode) return;

    try {
      if (this.audioCtx.state === "suspended") {
        this.audioCtx.resume();
      }

      const binary = atob(base64Data);
      const len = binary.length;
      const bytes = new Uint8Array(len);
      for (let i = 0; i < len; i++) {
        bytes[i] = binary.charCodeAt(i);
      }

      const floats = new Float32Array(bytes.buffer);
      const numSamples = floats.length / 2;
      if (numSamples === 0) return;

      const buffer = this.audioCtx.createBuffer(2, numSamples, 48000);
      const left = buffer.getChannelData(0);
      const right = buffer.getChannelData(1);

      for (let i = 0; i < numSamples; i++) {
        left[i] = floats[i * 2];
        right[i] = floats[i * 2 + 1];
      }

      const src = this.audioCtx.createBufferSource();
      src.buffer = buffer;
      src.connect(this.gainNode);

      const curTime = this.audioCtx.currentTime;
      if (this.nextAudioPlayTime < curTime) {
        this.nextAudioPlayTime = curTime;
      } else if (this.nextAudioPlayTime > curTime + 0.08) {
        // Limita o atraso máximo acumulado de áudio a 40ms para evitar descompasso
        this.nextAudioPlayTime = curTime + 0.04;
      }

      src.start(this.nextAudioPlayTime);
      this.nextAudioPlayTime += buffer.duration;
    } catch (e) {
      console.warn("Erro reproduzindo áudio PCM:", e);
    }
  }

  public setVolume(volume: number) {
    if (this.gainNode && this.audioCtx) {
      this.gainNode.gain.setValueAtTime(Math.max(0, Math.min(1, volume)), this.audioCtx.currentTime);
    }
  }

  public destroy() {
    if (this.videoDecoder) {
      try {
        if (this.videoDecoder.state !== "closed") {
          this.videoDecoder.close();
        }
      } catch (_) {}
      this.videoDecoder = null;
    }

    if (this.audioCtx) {
      try {
        this.audioCtx.close();
      } catch (_) {}
      this.audioCtx = null;
    }
  }
}
