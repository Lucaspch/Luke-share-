//! Player de vídeo de ultra-baixa latência usando WebCodecs (VideoDecoder) com recepção
//! IPC Binária direta (`ArrayBuffer` -> `Uint8Array` zero-copy, zero Base64).
//! O áudio é reproduzido nativamente no motor WASAPI em Rust (`NativeAudioPlayer`).

export interface PlayerStats {
  width: number;
  height: number;
  fps: number;
}

export type StreamEvent =
  | { t: "config"; width: number; height: number; fps: number; codec?: string }
  | { t: "audio_state"; enabled: boolean }
  | { t: "viewers"; count: number; viewers: string[] }
  | { t: "reconnecting"; attempt: number; remaining_secs: number }
  | { t: "reconnected" }
  | {
      t: "telemetry";
      rtt_ms: number;
      packets_rx: number;
      loss_pct: number;
      nacks_recovered: number;
      fec_recovered?: number;
      dropped_ref: number;
      fps: number;
      via_relay?: boolean;
    }
  | { t: "ended"; reason: string };

export type StreamChannelMsg = ArrayBuffer | Uint8Array | StreamEvent;

const BIN_VIDEO_MAGIC = 0x56; // 'V'
const BIN_VIDEO_FLAG_KEY = 0x01;
const BIN_VIDEO_HEADER_LEN = 8;

export function extractBinaryBuffer(msg: StreamChannelMsg): ArrayBuffer | null {
  if (msg instanceof ArrayBuffer) {
    return msg;
  }
  if (msg instanceof Uint8Array) {
    return msg.byteOffset === 0 && msg.byteLength === msg.buffer.byteLength
      ? (msg.buffer as ArrayBuffer)
      : (msg.slice().buffer as ArrayBuffer);
  }
  return null;
}

/**
 * Renderizador de Vídeo WebGL2 de Ultra-Baixa Latência (v0.5.2 - Feature 3.1).
 * Faz upload zero-copy de VideoFrames diretamente para textura GPU via D3D11/ANGLE,
 * evitando a sincronização e latência de apresentação de 2D Canvas.
 */
class NativeWebGLVideoPresenter {
  private gl: WebGL2RenderingContext | null = null;
  private program: WebGLProgram | null = null;
  private texture: WebGLTexture | null = null;
  private vao: WebGLVertexArrayObject | null = null;
  private buffer: WebGLBuffer | null = null;
  private isReady = false;

  constructor(canvas: HTMLCanvasElement) {
    try {
      const gl = canvas.getContext("webgl2", {
        alpha: false,
        desynchronized: true,
        powerPreference: "high-performance",
        preserveDrawingBuffer: false,
        antialias: false,
        depth: false,
        stencil: false,
      }) as WebGL2RenderingContext | null;

      if (!gl) return;
      this.gl = gl;

      const vsSource = `#version 300 es
        in vec2 a_pos;
        in vec2 a_tex;
        out vec2 v_tex;
        void main() {
          gl_Position = vec4(a_pos, 0.0, 1.0);
          v_tex = a_tex;
        }
      `;

      const fsSource = `#version 300 es
        precision mediump float;
        uniform sampler2D u_tex;
        in vec2 v_tex;
        out vec4 outColor;
        void main() {
          outColor = texture(u_tex, v_tex);
        }
      `;

      const vs = gl.createShader(gl.VERTEX_SHADER)!;
      gl.shaderSource(vs, vsSource);
      gl.compileShader(vs);

      const fs = gl.createShader(gl.FRAGMENT_SHADER)!;
      gl.shaderSource(fs, fsSource);
      gl.compileShader(fs);

      const prog = gl.createProgram()!;
      gl.attachShader(prog, vs);
      gl.attachShader(prog, fs);
      gl.linkProgram(prog);

      if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
        console.warn("WebGL2 shader link error:", gl.getProgramInfoLog(prog));
        return;
      }
      this.program = prog;

      const vao = gl.createVertexArray()!;
      gl.bindVertexArray(vao);
      this.vao = vao;

      // Geometria cobrindo todo o viewport: top-left (0,0) a bottom-right (1,1)
      const quad = new Float32Array([
        -1.0,  1.0,  0.0, 0.0,
        -1.0, -1.0,  0.0, 1.0,
         1.0,  1.0,  1.0, 0.0,
         1.0, -1.0,  1.0, 1.0,
      ]);

      const buf = gl.createBuffer()!;
      gl.bindBuffer(gl.ARRAY_BUFFER, buf);
      gl.bufferData(gl.ARRAY_BUFFER, quad, gl.STATIC_DRAW);
      this.buffer = buf;

      const aPos = gl.getAttribLocation(prog, "a_pos");
      const aTex = gl.getAttribLocation(prog, "a_tex");
      gl.enableVertexAttribArray(aPos);
      gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, 16, 0);
      gl.enableVertexAttribArray(aTex);
      gl.vertexAttribPointer(aTex, 2, gl.FLOAT, false, 16, 8);

      const tex = gl.createTexture()!;
      gl.bindTexture(gl.TEXTURE_2D, tex);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      this.texture = tex;

      this.isReady = true;
    } catch (e) {
      console.warn("Falha ao inicializar apresentador WebGL2 (usando fallback 2D):", e);
      this.isReady = false;
    }
  }

  public isAvailable(): boolean {
    return this.isReady && this.gl !== null;
  }

  public render(frame: VideoFrame, width: number, height: number): boolean {
    if (!this.isReady || !this.gl || !this.program || !this.texture || !this.vao) return false;
    const gl = this.gl;
    try {
      gl.viewport(0, 0, width, height);
      gl.useProgram(this.program);
      gl.bindVertexArray(this.vao);

      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, this.texture);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame);

      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      return true;
    } catch (_) {
      return false;
    }
  }

  public destroy() {
    if (!this.gl) return;
    const gl = this.gl;
    if (this.texture) gl.deleteTexture(this.texture);
    if (this.buffer) gl.deleteBuffer(this.buffer);
    if (this.vao) gl.deleteVertexArray(this.vao);
    if (this.program) gl.deleteProgram(this.program);
    this.isReady = false;
  }
}

export class StreamPlayer {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D | null = null;
  private webglPresenter: NativeWebGLVideoPresenter | null = null;
  private videoDecoder: VideoDecoder | null = null;
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
    _enableAudio = true,
    onRequestKey?: () => void
  ) {
    this.canvas = canvas;
    this.overlay = overlay;
    this.statsCallback = statsCallback;
    this.onRequestKey = onRequestKey;

    // Tenta apresentador WebGL2 direto na GPU (latência <1ms); se indisponível, fallback para 2D desynchronized
    this.webglPresenter = new NativeWebGLVideoPresenter(canvas);
    if (!this.webglPresenter.isAvailable()) {
      this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
    }
    this.initVideo();
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

        // Apresentação por hardware WebGL2 direta; fallback para 2D se necessário
        const presented = this.webglPresenter ? this.webglPresenter.render(frame, this.canvas.width, this.canvas.height) : false;
        if (!presented && this.ctx) {
          this.ctx.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
        }
        frame.close();

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
        configured = true;
        break;
      } catch (_) {}
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
  }

  /**
   * Recebe o pacote binário puro do IPC do Tauri v2 (`ArrayBuffer`), sem Base64 e sem cópias extras:
   * `[0x56, flags, 0, 0, pts_u32_le(4B), ...bitstream]`
   */
  public handleBinaryVideoPacket(buf: ArrayBuffer) {
    if (buf.byteLength <= BIN_VIDEO_HEADER_LEN) return;
    const view = new DataView(buf);
    if (view.getUint8(0) !== BIN_VIDEO_MAGIC) return;

    const isKey = (view.getUint8(1) & BIN_VIDEO_FLAG_KEY) !== 0;
    const pts = view.getUint32(4, true);
    const bitstream = new Uint8Array(buf, BIN_VIDEO_HEADER_LEN);

    this.decodeBitstream(bitstream, isKey, pts);
  }

  public decodeBitstream(bytes: Uint8Array, isKey: boolean, pts: number) {
    if (!this.videoDecoder || this.videoDecoder.state !== "configured") {
      this.initVideo();
      if (!this.videoDecoder || this.videoDecoder.state !== "configured") return;
    }

    if (this.videoDecoder.decodeQueueSize > 36) {
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

    if (!this.hasFirstKeyFrame) {
      if (!isKey) {
        return;
      }
      this.hasFirstKeyFrame = true;
    }

    try {
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

  public markAwaitingKeyFrame() {
    this.hasFirstKeyFrame = false;
  }

  public destroy() {
    if (this.webglPresenter) {
      this.webglPresenter.destroy();
      this.webglPresenter = null;
    }

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
  }
}
