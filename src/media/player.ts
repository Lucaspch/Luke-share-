//! Player de mídia de ultra-baixa latência usando WebCodecs (VideoDecoder) e Web Audio API (PCM f32).

export interface PlayerStats {
  width: number;
  height: number;
  fps: number;
}

export type StreamEvent =
  | { t: "video"; k: boolean; p: number; d: string }
  | { t: "audio"; d: string }
  | { t: "config"; width: number; height: number; fps: number; codec?: string }
  | { t: "audio_state"; enabled: boolean }
  | {
      t: "telemetry";
      rtt_ms: number;
      packets_rx: number;
      loss_pct: number;
      nacks_recovered: number;
      fec_recovered?: number;
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
  private hasFirstDecodedFrame = false;
  private currentCodec = "h264";
  private pipVideo: HTMLVideoElement | null = null;
  private isPiP = false;

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
    this.hasFirstDecodedFrame = false;
    this.videoDecoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        if (this.overlay && !this.overlay.classList.contains("hidden")) {
          this.overlay.classList.add("hidden");
        }

        const w = frame.displayWidth || frame.codedWidth || this.currentWidth;
        const h = frame.displayHeight || frame.codedHeight || this.currentHeight;
        if (w > 0 && h > 0) {
          this.currentWidth = w;
          this.currentHeight = h;
          if (this.canvas.width !== w || this.canvas.height !== h) {
            this.canvas.width = w;
            this.canvas.height = h;
          }
        }

        this.ctx.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
        frame.close();

        // Notifica estatísticas no primeiro frame para evitar '0x0'
        if (!this.hasFirstDecodedFrame) {
          this.hasFirstDecodedFrame = true;
          if (this.statsCallback && this.currentWidth > 0 && this.currentHeight > 0) {
            this.statsCallback({
              width: this.currentWidth,
              height: this.currentHeight,
              fps: this.currentFps || 60,
            });
          }
        }

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

    this.configureDecoder(this.currentCodec);
  }

  public setDimensions(width: number, height: number) {
    if (width > 0) this.currentWidth = width;
    if (height > 0) this.currentHeight = height;
    if (this.canvas.width !== this.currentWidth || this.canvas.height !== this.currentHeight) {
      if (this.currentWidth > 0 && this.currentHeight > 0) {
        this.canvas.width = this.currentWidth;
        this.canvas.height = this.currentHeight;
      }
    }
    if (this.statsCallback && this.currentWidth > 0 && this.currentHeight > 0) {
      this.statsCallback({
        width: this.currentWidth,
        height: this.currentHeight,
        fps: this.currentFps,
      });
    }
  }

  public configureDecoder(codecName?: string) {
    if (codecName) {
      this.currentCodec = codecName.toLowerCase().trim();
    }
    if (!this.videoDecoder) return;

    const candidates: string[] = [];
    if (this.currentCodec === "av1") {
      candidates.push("av01.0.08M.08", "av01.0.05M.08", "av01.0.04M.08");
    } else if (this.currentCodec === "hevc" || this.currentCodec === "h265") {
      candidates.push("hev1.1.6.L120.90", "hev1.1.6.L93.B0", "hvc1.1.6.L120.90", "hvc1.1.6.L93.B0");
    } else {
      candidates.push("avc1.640034", "avc1.64002a", "avc1.4d4028", "avc1.42e01f");
    }

    let configured = false;
    for (const codecString of candidates) {
      try {
        this.videoDecoder.configure({
          codec: codecString,
          optimizeForLatency: true,
          hardwareAcceleration: "prefer-hardware",
        });
        console.log(`VideoDecoder configurado com sucesso para: ${codecString}`);
        configured = true;
        break;
      } catch (e) {
        console.warn(`Tentativa com codec ${codecString} falhou, tentando próximo:`, e);
      }
    }

    if (!configured) {
      try {
        this.videoDecoder.configure({
          codec: "avc1.64002a",
          optimizeForLatency: true,
          hardwareAcceleration: "prefer-hardware",
        });
      } catch (e) {
        console.error("Falha ao configurar VideoDecoder:", e);
      }
    }
  }

  public isPiPActive(): boolean {
    return this.isPiP || document.pictureInPictureElement !== null;
  }

  public async togglePiP(): Promise<boolean> {
    if (document.pictureInPictureElement) {
      await document.exitPictureInPicture();
      this.isPiP = false;
      return false;
    }

    try {
      if (!this.pipVideo) {
        this.pipVideo = document.createElement("video");
        this.pipVideo.muted = true;
        this.pipVideo.playsInline = true;
        this.pipVideo.autoplay = true;
        this.pipVideo.style.position = "fixed";
        this.pipVideo.style.top = "-9999px";
        this.pipVideo.style.left = "-9999px";
        this.pipVideo.style.width = "1px";
        this.pipVideo.style.height = "1px";
        this.pipVideo.style.opacity = "0";
        this.pipVideo.style.pointerEvents = "none";
        document.body.appendChild(this.pipVideo);

        this.pipVideo.addEventListener("leavepictureinpicture", () => {
          this.isPiP = false;
        });
      }

      const stream = (this.canvas as any).captureStream ? (this.canvas as any).captureStream(60) : null;
      if (!stream) {
        throw new Error("captureStream() não suportado no canvas deste navegador");
      }

      this.pipVideo.srcObject = stream;
      await this.pipVideo.play();
      await this.pipVideo.requestPictureInPicture();
      this.isPiP = true;
      return true;
    } catch (e) {
      console.warn("Erro ao iniciar Picture-in-Picture:", e);
      throw e;
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

    if (this.pipVideo) {
      if (document.pictureInPictureElement === this.pipVideo) {
        document.exitPictureInPicture().catch(() => {});
      }
      this.pipVideo.remove();
      this.pipVideo = null;
    }

    if (this.audioCtx) {
      try {
        this.audioCtx.close();
      } catch (_) {}
      this.audioCtx = null;
    }
  }
}
