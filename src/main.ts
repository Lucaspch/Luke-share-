import { invoke, Channel } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { PeersManager } from "./ui/peers";
import { ShareManager } from "./ui/share";
import { StreamPlayer } from "./media/player";
import { UpdaterManager } from "./ui/updater";

interface MyInfo {
  id: string;
  nick: string;
  status: "Idle" | "Streaming" | "Watching";
  ip: string | null;
  adapter_name: string | null;
}

type StreamEvent =
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

class App {
  private peersManager!: PeersManager;
  private shareManager!: ShareManager;
  private updaterManager!: UpdaterManager;
  private player: StreamPlayer | null = null;
  private myInfo: MyInfo | null = null;
  private isMiniPlayer = false;

  // Header Elements
  private myIpEl = document.getElementById("my-ip") as HTMLElement;
  private myStatusEl = document.getElementById("my-status") as HTMLElement;
  private nickInput = document.getElementById("nick-input") as HTMLInputElement;
  private btnSaveNick = document.getElementById("btn-save-nick") as HTMLButtonElement;

  // Panels
  private sharePanel = document.getElementById("share-panel") as HTMLElement;
  private watchPanel = document.getElementById("watch-panel") as HTMLElement;

  // Watch Elements
  private watchHostName = document.getElementById("watch-host-name") as HTMLElement;
  private watchMetaInfo = document.getElementById("watch-meta-info") as HTMLElement;
  private btnStopWatch = document.getElementById("btn-stop-watch") as HTMLButtonElement;
  private btnFullscreen = document.getElementById("btn-fullscreen") as HTMLButtonElement;
  private btnPip = document.getElementById("btn-pip") as HTMLButtonElement | null;
  private btnMiniPlayer = document.getElementById("btn-mini-player") as HTMLButtonElement | null;
  private btnExitMiniPlayer = document.getElementById("btn-exit-mini-player") as HTMLButtonElement | null;
  private canvasContainer = document.getElementById("canvas-container") as HTMLElement;
  private streamCanvas = document.getElementById("stream-canvas") as HTMLCanvasElement;
  private playerOverlay = document.getElementById("player-overlay") as HTMLElement;
  private volumeSlider = document.getElementById("volume-slider") as HTMLInputElement;
  private btnPlayerMute = document.getElementById("btn-player-mute") as HTMLButtonElement;
  private playerResTag = document.getElementById("player-res-tag") as HTMLElement;
  private playerFpsTag = document.getElementById("player-fps-tag") as HTMLElement;
  private playerRttTag = document.getElementById("player-rtt-tag") as HTMLElement;
  private playerLossTag = document.getElementById("player-loss-tag") as HTMLElement;

  constructor() {
    this.init();
  }

  private async init() {
    await this.fetchMyInfo();
    this.initPeers();
    this.initShare();
    this.updaterManager = new UpdaterManager();
    this.bindEvents();
  }

  private async fetchMyInfo() {
    try {
      const info = await invoke<MyInfo>("get_my_info");
      this.myInfo = info;
      this.updateMyInfoUI();
    } catch (e) {
      console.warn("Erro ao obter informações locais:", e);
    }
  }

  private updateMyInfoUI() {
    if (!this.myInfo) return;

    if (this.myInfo.ip) {
      this.myIpEl.textContent = `${this.myInfo.ip} • P2P Pronto`;
      this.myIpEl.title = `Rede Local (${this.myInfo.ip}) + Iroh P2P Nativo`;
    } else {
      this.myIpEl.textContent = "P2P Nativo Pronto";
      this.myIpEl.title = "Iroh P2P Nativo (Internet Direta sem VPN).";
    }

    this.nickInput.value = this.myInfo.nick;
    this.updateStatusPill(this.myInfo.status);
  }

  private updateStatusPill(status: "Idle" | "Streaming" | "Watching") {
    const labels = {
      Idle: "Ocioso",
      Streaming: "🔴 Transmitindo",
      Watching: "🟡 Assistindo",
    };

    this.myStatusEl.className = "status-pill";
    if (status === "Streaming") this.myStatusEl.classList.add("streaming");
    if (status === "Watching") this.myStatusEl.classList.add("watching");
    this.myStatusEl.textContent = labels[status] || status;
  }

  private initPeers() {
    const peersContainer = document.getElementById("peers-list") as HTMLElement;
    const peersCountBadge = document.getElementById("peers-count") as HTMLElement;
    const manualIpInput = document.getElementById("manual-ip-input") as HTMLInputElement;
    const btnAddManual = document.getElementById("btn-add-manual") as HTMLButtonElement;

    this.peersManager = new PeersManager(
      peersContainer,
      peersCountBadge,
      manualIpInput,
      btnAddManual,
      (ip, nick) => this.startWatchingPeer(ip, nick)
    );
  }

  private initShare() {
    this.shareManager = new ShareManager(
      {
        sourcesGrid: document.getElementById("sources-grid") as HTMLElement,
        btnRefresh: document.getElementById("btn-refresh-sources") as HTMLButtonElement,
        tabButtons: document.querySelectorAll<HTMLButtonElement>(".tab-btn"),
        resolutionSelect: document.getElementById("resolution-select") as HTMLSelectElement,
        fpsSelect: document.getElementById("fps-select") as HTMLSelectElement,
        bitrateSelect: document.getElementById("bitrate-select") as HTMLSelectElement,
        codecSelect: document.getElementById("codec-select") as HTMLSelectElement,
        sourcePreviewImg: document.getElementById("source-preview-img") as HTMLImageElement,
        sourcePreviewPlaceholder: document.getElementById("source-preview-placeholder") as HTMLElement,
        btnRefreshPreview: document.getElementById("btn-refresh-preview") as HTMLButtonElement,
        audioCheckbox: document.getElementById("audio-enable-checkbox") as HTMLInputElement,
        btnStart: document.getElementById("btn-start-share") as HTMLButtonElement,
        btnStop: document.getElementById("btn-stop-share") as HTMLButtonElement,
        btnStopTop: document.getElementById("btn-stop-share-top") as HTMLButtonElement,
        sourcesSelectionArea: document.getElementById("sources-selection-area") as HTMLElement,
        hostPreviewCard: document.getElementById("host-preview-card") as HTMLElement,
        hostPreviewCanvas: document.getElementById("host-preview-canvas") as HTMLCanvasElement,
        hostPreviewOverlay: document.getElementById("host-preview-overlay") as HTMLElement,
        previewFpsTag: document.getElementById("preview-fps-tag") as HTMLElement,
        previewResTag: document.getElementById("preview-res-tag") as HTMLElement,
        previewViewersCount: document.getElementById("preview-viewers-count") as HTMLElement,
        statPort: document.getElementById("stat-port") as HTMLElement,
        statAudioTag: document.getElementById("stat-audio-tag") as HTMLElement,
        btnToggleMute: document.getElementById("btn-toggle-audio-mute") as HTMLButtonElement,
        roomTicketBox: document.getElementById("room-ticket-box") as HTMLElement,
        shareTicketInput: document.getElementById("share-ticket-input") as HTMLInputElement,
        btnCopyTicket: document.getElementById("btn-copy-ticket") as HTMLButtonElement,
      },
      (streaming) => {
        this.updateStatusPill(streaming ? "Streaming" : "Idle");
      }
    );
  }

  private bindEvents() {
    // Salvar Nickname
    const handleSaveNick = async () => {
      const nick = this.nickInput.value.trim();
      if (!nick) return;
      try {
        await invoke("set_my_nick", { nick });
        if (this.myInfo) this.myInfo.nick = nick;
      } catch (e) {
        alert("Erro ao salvar apelido: " + e);
      }
    };
    this.btnSaveNick.addEventListener("click", handleSaveNick);
    this.nickInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleSaveNick();
    });

    // Desconectar do stream assistido
    this.btnStopWatch.addEventListener("click", () => this.stopWatching());

    // Picture-in-Picture (Janela flutuante nativa do sistema)
    if (this.btnPip) {
      this.btnPip.addEventListener("click", async () => {
        if (!this.player) return;
        try {
          const active = await this.player.togglePiP();
          if (this.btnPip) {
            this.btnPip.textContent = active ? "📌 Sair PiP" : "📌 PiP";
          }
        } catch (e) {
          alert("Picture-in-Picture não suportado ou negado pelo sistema: " + e);
        }
      });
    }

    // Modo Mini-Player (Compacto & Always on Top)
    const toggleMiniPlayer = async () => {
      try {
        const win = getCurrentWindow();
        this.isMiniPlayer = !this.isMiniPlayer;
        if (this.isMiniPlayer) {
          document.body.classList.add("mini-player-active");
          await win.setAlwaysOnTop(true);
          await win.setSize(new LogicalSize(480, 290));
          if (this.btnMiniPlayer) this.btnMiniPlayer.textContent = "🪟 Normal";
        } else {
          document.body.classList.remove("mini-player-active");
          await win.setAlwaysOnTop(false);
          await win.setSize(new LogicalSize(1100, 720));
          if (this.btnMiniPlayer) this.btnMiniPlayer.textContent = "🪟 Mini-Player";
        }
      } catch (e) {
        console.warn("Falha ao alternar modo Mini-Player:", e);
      }
    };

    if (this.btnMiniPlayer) {
      this.btnMiniPlayer.addEventListener("click", toggleMiniPlayer);
    }
    if (this.btnExitMiniPlayer) {
      this.btnExitMiniPlayer.addEventListener("click", toggleMiniPlayer);
    }

    // Tela cheia
    this.btnFullscreen.addEventListener("click", () => {
      if (!document.fullscreenElement) {
        this.canvasContainer.requestFullscreen().catch((err) => {
          console.warn("Fullscreen falhou:", err);
        });
      } else {
        document.exitFullscreen();
      }
    });

    // Controle de volume do player
    this.volumeSlider.addEventListener("input", () => {
      const vol = parseFloat(this.volumeSlider.value);
      if (this.player) {
        this.player.setVolume(vol);
      }
      this.btnPlayerMute.textContent = vol === 0 ? "🔇" : "🔊";
    });

    this.btnPlayerMute.addEventListener("click", () => {
      if (parseFloat(this.volumeSlider.value) > 0) {
        this.volumeSlider.dataset.prev = this.volumeSlider.value;
        this.volumeSlider.value = "0";
      } else {
        this.volumeSlider.value = this.volumeSlider.dataset.prev || "1";
      }
      this.volumeSlider.dispatchEvent(new Event("input"));
    });
  }

  private async startWatchingPeer(ip: string, nick: string) {
    // Se estava transmitindo, para primeiro
    await this.shareManager.stopShare();

    const isTicket = ip.startsWith("lk-");
    const displayHost = isTicket ? "Sala P2P (Internet)" : nick;
    const displayAddr = isTicket ? "P2P Iroh (Internet Direta)" : ip;

    this.watchHostName.textContent = displayHost;
    this.watchMetaInfo.textContent = `${displayAddr} • Conectando...`;
    this.playerOverlay.classList.remove("hidden");

    // Alterna visualização para o player
    this.sharePanel.classList.add("hidden");
    this.watchPanel.classList.remove("hidden");
    this.updateStatusPill("Watching");

    // Inicializa o player de baixa latência
    if (this.player) {
      this.player.destroy();
    }
    this.player = new StreamPlayer(
      this.streamCanvas,
      this.playerOverlay,
      (stats) => {
        this.playerResTag.textContent = `${stats.width}x${stats.height}`;
        this.playerFpsTag.textContent = `${stats.fps} FPS`;
      },
      true,
      () => {
        invoke("request_stream_keyframe").catch(() => {});
      }
    );

    // Canal Tauri v2 IPC para receber pacotes de vídeo e áudio
    const onEvent = new Channel<StreamEvent>();
    onEvent.onmessage = (msg: StreamEvent) => {
      if (!this.player) return;

      switch (msg.t) {
        case "video":
          this.player.handleVideoFrame(msg.d, msg.k, msg.p);
          break;
        case "audio":
          this.player.handleAudioChunk(msg.d);
          break;
        case "config": {
          if (msg.codec) {
            this.player.configureDecoder(msg.codec);
          }
          const codecBadge = (msg.codec || "H.264").toUpperCase();
          this.watchMetaInfo.textContent = `${displayAddr} • ${msg.width}x${msg.height} @ ${msg.fps} FPS (${codecBadge})`;
          this.playerResTag.textContent = `${msg.width}x${msg.height}`;
          break;
        }
        case "telemetry":
          this.playerFpsTag.textContent = `${msg.fps} FPS`;
          if (this.playerRttTag) {
            this.playerRttTag.textContent = `RTT: ${msg.rtt_ms}ms`;
          }
          if (this.playerLossTag) {
            const fecInfo = (msg.fec_recovered && msg.fec_recovered > 0) ? ` • FEC: ${msg.fec_recovered}` : "";
            this.playerLossTag.textContent = `Perda: ${msg.loss_pct.toFixed(1)}% (NACKs: ${msg.nacks_recovered}${fecInfo})`;
            if (msg.loss_pct > 2.0 || msg.dropped_ref > 0) {
              this.playerLossTag.classList.add("warning");
            } else {
              this.playerLossTag.classList.remove("warning");
            }
          }
          break;
        case "audio_state":
          break;
        case "ended":
          alert(`A transmissão foi encerrada: ${msg.reason}`);
          this.stopWatching();
          break;
      }
    };

    try {
      await invoke("watch_peer", { peerIp: ip, onEvent });
      this.watchMetaInfo.textContent = `${displayAddr} • Transmissão Ativa`;
    } catch (e) {
      alert("Falha ao conectar na transmissão: " + e);
      this.stopWatching();
    }
  }

  private async stopWatching() {
    try {
      await invoke("stop_watching");
    } catch (e) {
      console.warn("Erro ao desconectar:", e);
    } finally {
      if (this.isMiniPlayer) {
        this.isMiniPlayer = false;
        document.body.classList.remove("mini-player-active");
        try {
          const win = getCurrentWindow();
          await win.setAlwaysOnTop(false);
          await win.setSize(new LogicalSize(1100, 720));
        } catch (_) {}
      }
      if (this.btnPip) {
        this.btnPip.textContent = "📌 PiP";
      }
      if (this.btnMiniPlayer) {
        this.btnMiniPlayer.textContent = "🪟 Mini-Player";
      }
      if (this.player) {
        this.player.destroy();
        this.player = null;
      }
      if (this.playerRttTag) this.playerRttTag.textContent = "RTT: -";
      if (this.playerLossTag) {
        this.playerLossTag.textContent = "Perda: 0%";
        this.playerLossTag.classList.remove("warning");
      }
      this.watchPanel.classList.add("hidden");
      this.sharePanel.classList.remove("hidden");
      this.updateStatusPill("Idle");
    }
  }
}

// Inicializa a aplicação ao carregar o DOM
window.addEventListener("DOMContentLoaded", () => {
  new App();
});
