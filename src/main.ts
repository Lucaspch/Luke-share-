import { invoke, Channel } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { PeersManager } from "./ui/peers";
import { ShareManager } from "./ui/share";
import { StreamPlayer, StreamEvent } from "./media/player";
import { UpdaterManager } from "./ui/updater";

interface MyInfo {
  id: string;
  nick: string;
  status: "Idle" | "Streaming" | "Watching";
  ip: string | null;
  adapter_name: string | null;
}

interface ActiveStream {
  id: string;
  nick: string;
  addr: string;
  player: StreamPlayer;
  element: HTMLElement;
  statBadge: HTMLElement;
  volumeSlider: HTMLInputElement;
  btnMute: HTMLButtonElement;
}

class App {
  private peersManager!: PeersManager;
  private shareManager!: ShareManager;
  private updaterManager!: UpdaterManager;
  private myInfo: MyInfo | null = null;
  private isMiniPlayer = false;

  // Mapa de transmissões ativas sendo assistidas (multi-stream)
  private activeStreams = new Map<string, ActiveStream>();
  private focusedStreamId: string | null = null;

  // Stream preview do Host na Grade (quando anfitrião está na chamada)
  private hostTileElement: HTMLElement | null = null;
  private hostTilePlayer: StreamPlayer | null = null;

  // Elementos do Cabeçalho
  private myIpEl = document.getElementById("my-ip") as HTMLElement;
  private myStatusEl = document.getElementById("my-status") as HTMLElement;
  private nickInput = document.getElementById("nick-input") as HTMLInputElement;
  private btnSaveNick = document.getElementById("btn-save-nick") as HTMLButtonElement;

  // Painéis Principais
  private sharePanel = document.getElementById("share-panel") as HTMLElement;
  private watchPanel = document.getElementById("watch-panel") as HTMLElement;

  // Elementos da Chamada / Multi-Stream (Watch Panel)
  private streamsGridWrapper = document.getElementById("streams-grid-wrapper") as HTMLElement;
  private streamsGrid = document.getElementById("streams-grid") as HTMLElement;
  private streamsCountBadge = document.getElementById("streams-count-badge") as HTMLElement;
  private btnCallShare = document.getElementById("btn-call-share") as HTMLButtonElement;
  private btnCallGridMode = document.getElementById("btn-call-grid-mode") as HTMLButtonElement;
  private btnStopAllWatch = document.getElementById("btn-stop-all-watch") as HTMLButtonElement;
  private btnFullscreen = document.getElementById("btn-fullscreen") as HTMLButtonElement;
  private btnMiniPlayer = document.getElementById("btn-mini-player") as HTMLButtonElement | null;
  private btnExitMiniPlayer = document.getElementById("btn-exit-mini-player") as HTMLButtonElement | null;

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
    this.updateStatusPill();
  }

  private updateStatusPill() {
    const isSharing = this.shareManager?.getIsSharing() || false;
    const watchCount = this.activeStreams.size;
    this.myStatusEl.className = "status-pill";

    if (isSharing && watchCount > 0) {
      this.myStatusEl.classList.add("streaming");
      this.myStatusEl.textContent = `🔴 Transmitindo + 🟡 Assistindo (${watchCount})`;
    } else if (isSharing) {
      this.myStatusEl.classList.add("streaming");
      this.myStatusEl.textContent = "🔴 Transmitindo";
    } else if (watchCount > 0) {
      this.myStatusEl.classList.add("watching");
      this.myStatusEl.textContent = `🟡 Assistindo (${watchCount})`;
    } else {
      this.myStatusEl.textContent = "Ocioso";
    }
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
        tabButtons: document.querySelectorAll<HTMLButtonElement>("#share-panel .tab-btn"),
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
        modal: {
          modalBackdrop: document.getElementById("share-modal") as HTMLElement,
          btnClose: document.getElementById("btn-close-share-modal") as HTMLButtonElement,
          btnCancel: document.getElementById("btn-cancel-modal-share") as HTMLButtonElement,
          btnStart: document.getElementById("btn-start-modal-share") as HTMLButtonElement,
          tabButtons: document.querySelectorAll<HTMLButtonElement>(".modal-tab-btn"),
          sourcesGrid: document.getElementById("modal-sources-grid") as HTMLElement,
          sourcePreviewImg: document.getElementById("modal-source-preview-img") as HTMLImageElement,
          sourcePreviewPlaceholder: document.getElementById("modal-source-preview-placeholder") as HTMLElement,
          btnRefreshPreview: document.getElementById("btn-refresh-modal-preview") as HTMLButtonElement,
          codecSelect: document.getElementById("modal-codec-select") as HTMLSelectElement,
          resolutionSelect: document.getElementById("modal-resolution-select") as HTMLSelectElement,
          fpsSelect: document.getElementById("modal-fps-select") as HTMLSelectElement,
          bitrateSelect: document.getElementById("modal-bitrate-select") as HTMLSelectElement,
          audioCheckbox: document.getElementById("modal-audio-enable-checkbox") as HTMLInputElement,
        },
      },
      (streaming) => {
        if (streaming) {
          this.btnCallShare.classList.add("active-share");
          this.btnCallShare.innerHTML = '<span class="call-icon">🔴</span>';
          this.btnCallShare.title = "Você está transmitindo tela (Clique para ver/copiar código)";

          // Se estiver na tela da chamada, renderiza o Host Tile na grade
          if (!this.watchPanel.classList.contains("hidden")) {
            this.renderHostTile();
          }
        } else {
          this.btnCallShare.classList.remove("active-share");
          this.btnCallShare.innerHTML = '<span class="call-icon">🖥️</span>';
          this.btnCallShare.title = "Compartilhar sua tela";
          this.removeHostTile();
        }
        this.updateStatusPill();
        this.updateGridLayout();
      }
    );

    // Atualiza contagem de espectadores e tráfego na miniatura do host na grade
    this.shareManager.onViewersCountChange = (_count, text) => {
      const hostViewersEl = document.getElementById("host-tile-viewers");
      if (hostViewersEl) {
        hostViewersEl.textContent = `👥 ${text}`;
      }
    };

    // Encaminha pacotes de pré-visualização para o host tile quando ativo na grade
    this.shareManager.onPreviewStreamEvent = (msg: StreamEvent) => {
      if (!this.hostTilePlayer) return;
      if (msg.t === "video") {
        this.hostTilePlayer.handleVideoFrame(msg.d, msg.k, msg.p);
      } else if (msg.t === "config") {
        if (msg.width && msg.height) {
          this.hostTilePlayer.setDimensions(msg.width, msg.height);
        }
        if (msg.codec) {
          this.hostTilePlayer.configureDecoder(msg.codec);
        }
      }
    };
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

    // Botão de Transmitir Tela dentro da chamada (Discord style)
    this.btnCallShare.addEventListener("click", () => {
      if (this.shareManager.getIsSharing()) {
        const ticket = this.shareManager.getRoomTicket();
        if (ticket) {
          navigator.clipboard.writeText(ticket);
          alert(`Você já está transmitindo!\nCódigo da Sala copiado para a Área de Transferência:\n\n${ticket}`);
        } else {
          alert("Você já está transmitindo ao vivo!");
        }
      } else {
        this.shareManager.openShareModal();
      }
    });

    // Copiar Código da Sala no Dock da Chamada
    const btnCallCopyTicket = document.getElementById("btn-call-copy-ticket") as HTMLButtonElement | null;
    if (btnCallCopyTicket) {
      btnCallCopyTicket.addEventListener("click", () => {
        const ticket = this.shareManager.getRoomTicket();
        if (ticket) {
          navigator.clipboard.writeText(ticket);
          const originalContent = btnCallCopyTicket.innerHTML;
          btnCallCopyTicket.innerHTML = `<span class="call-icon">✅</span><span class="call-btn-label">Copiado!</span>`;
          setTimeout(() => {
            btnCallCopyTicket.innerHTML = originalContent;
          }, 2000);
        } else {
          alert("Nenhum código de sala ativo no momento.\nInicie uma transmissão para gerar um código e convidar amigos!");
        }
      });
    }

    // Mutar/Desmutar Microfone / Áudio do Sistema na Chamada
    const btnCallMuteMic = document.getElementById("btn-call-mute-mic") as HTMLButtonElement | null;
    const callMicIcon = document.getElementById("call-mic-icon") as HTMLElement | null;
    if (btnCallMuteMic) {
      btnCallMuteMic.addEventListener("click", async () => {
        const muted = await this.shareManager.toggleAudioMute();
        if (callMicIcon) {
          callMicIcon.textContent = muted ? "🔇" : "🎙️";
        }
        btnCallMuteMic.title = muted ? "Desmutar áudio do sistema" : "Mutar áudio do sistema";
        if (muted) {
          btnCallMuteMic.classList.add("btn-call-muted");
        } else {
          btnCallMuteMic.classList.remove("btn-call-muted");
        }
      });
    }

    // Alternar Grade / Foco
    this.btnCallGridMode.addEventListener("click", () => {
      if (this.focusedStreamId) {
        this.clearStreamFocus();
      } else if (this.activeStreams.size > 0) {
        const firstId = this.activeStreams.keys().next().value;
        if (firstId) {
          this.setStreamFocus(firstId);
        }
      } else if (this.hostTileElement) {
        this.setStreamFocus("host");
      }
    });

    // Desconectar de todas as transmissões (Sair da Chamada)
    this.btnStopAllWatch.addEventListener("click", () => this.stopAllWatching());

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
        this.streamsGridWrapper.requestFullscreen().catch((err) => {
          console.warn("Fullscreen falhou:", err);
        });
      } else {
        document.exitFullscreen();
      }
    });
  }

  /**
   * Conecta a um amigo (ou código de sala lk-...) e adiciona uma tile à Grade da Chamada.
   * Não encerra sua própria transmissão se estiver ativo!
   */
  private async startWatchingPeer(ip: string, nick: string) {
    const streamId = ip.trim();

    // Se já estiver assistindo este stream, foca nele
    if (this.activeStreams.has(streamId)) {
      this.setStreamFocus(streamId);
      return;
    }

    const isTicket = streamId.startsWith("lk-");
    const displayHost = isTicket ? `Amigo (${nick === "Sala P2P" ? "P2P" : nick})` : nick;
    const displayAddr = isTicket ? "P2P Internet" : streamId;

    // Alterna visualização para o Watch Panel da Chamada (sem parar transmissão ativa)
    this.sharePanel.classList.add("hidden");
    this.watchPanel.classList.remove("hidden");

    // Se estiver transmitindo, assegura que a sua miniatura esteja presente na grade
    if (this.shareManager.getIsSharing()) {
      this.renderHostTile();
    }

    // Cria o elemento Tile na grade
    const tile = document.createElement("div");
    tile.className = "stream-tile";
    tile.dataset.streamId = streamId;

    tile.innerHTML = `
      <div class="stream-tile-canvas-wrap">
        <canvas class="stream-tile-canvas"></canvas>
        <div class="player-overlay">
          <div class="spinner"></div>
          <div class="overlay-text">Conectando à transmissão...</div>
        </div>
      </div>

      <div class="tile-top-overlay">
        <span class="badge-discord-live">🔴 LIVE</span>
        <span class="badge-tile-stat tile-stat-badge">Conectando...</span>
      </div>

      <div class="tile-user-pill">
        <span class="tile-user-icon">🖥️</span>
        <span class="tile-user-name" title="${this.escapeHtml(displayHost)}">${this.escapeHtml(displayHost)}</span>
      </div>

      <div class="tile-actions-overlay">
        <div class="tile-volume-popover">
          <button class="btn-tile-icon btn-tile-mute" title="Mutar áudio">🔊</button>
          <input type="range" class="tile-volume-slider" min="0" max="1" step="0.05" value="1" title="Volume individual" />
        </div>
        <button class="btn-tile-icon btn-tile-pip" title="Picture-in-Picture">📌</button>
        <button class="btn-tile-icon btn-tile-focus" title="Focar/Expandir este stream">⛶</button>
        <button class="btn-tile-icon btn-tile-close" title="Desconectar desta transmissão">✕</button>
      </div>
    `;

    const canvas = tile.querySelector("canvas") as HTMLCanvasElement;
    const overlay = tile.querySelector(".player-overlay") as HTMLElement;
    const statBadge = tile.querySelector(".tile-stat-badge") as HTMLElement;
    const volumeSlider = tile.querySelector(".tile-volume-slider") as HTMLInputElement;
    const btnMute = tile.querySelector(".btn-tile-mute") as HTMLButtonElement;
    const btnPip = tile.querySelector(".btn-tile-pip") as HTMLButtonElement;
    const btnFocus = tile.querySelector(".btn-tile-focus") as HTMLButtonElement;
    const btnClose = tile.querySelector(".btn-tile-close") as HTMLButtonElement;

    let currentRes = "1080p";
    let currentFps = "60 FPS";
    let currentCodec = "H.264";

    // Inicializa o StreamPlayer dedicado para este tile
    const player = new StreamPlayer(
      canvas,
      overlay,
      (stats) => {
        currentRes = `${stats.width}x${stats.height}`;
        currentFps = `${stats.fps} FPS`;
        statBadge.textContent = `${currentRes} • ${currentFps}`;
      },
      true,
      () => {
        invoke("request_stream_keyframe", { streamId }).catch(() => {});
      }
    );

    // Canal Tauri v2 IPC para pacotes de vídeo e áudio deste stream
    const onEvent = new Channel<StreamEvent>();
    onEvent.onmessage = (msg: StreamEvent) => {
      switch (msg.t) {
        case "video":
          player.handleVideoFrame(msg.d, msg.k, msg.p);
          break;
        case "audio":
          player.handleAudioChunk(msg.d);
          break;
        case "config":
          if (msg.width && msg.height) {
            player.setDimensions(msg.width, msg.height);
            currentRes = `${msg.width}x${msg.height}`;
          }
          if (msg.fps) {
            currentFps = `${msg.fps} FPS`;
          }
          if (msg.codec) {
            player.configureDecoder(msg.codec);
            currentCodec = msg.codec.toUpperCase();
          }
          statBadge.textContent = `${currentRes} • ${currentFps} • ${currentCodec}`;
          break;
        case "telemetry":
          currentFps = `${msg.fps} FPS`;
          const fecInfo = msg.fec_recovered ? ` • FEC:${msg.fec_recovered}` : "";
          const lossInfo = msg.loss_pct > 1 ? ` • ${msg.loss_pct.toFixed(1)}% perda` : "";
          statBadge.textContent = `${currentRes} • ${currentFps} • ${msg.rtt_ms}ms${fecInfo}${lossInfo}`;
          break;
        case "ended":
          this.stopWatchingPeer(streamId);
          break;
      }
    };

    // Controle de volume individual (estilo Discord)
    volumeSlider.addEventListener("input", () => {
      const vol = parseFloat(volumeSlider.value);
      player.setVolume(vol);
      btnMute.textContent = vol === 0 ? "🔇" : "🔊";
    });

    btnMute.addEventListener("click", () => {
      if (parseFloat(volumeSlider.value) > 0) {
        volumeSlider.dataset.prev = volumeSlider.value;
        volumeSlider.value = "0";
      } else {
        volumeSlider.value = volumeSlider.dataset.prev || "1";
      }
      volumeSlider.dispatchEvent(new Event("input"));
    });

    // Foco / Teatro
    btnFocus.addEventListener("click", () => {
      this.toggleFocusStream(streamId);
    });

    // PiP
    btnPip.addEventListener("click", async () => {
      try {
        await player.togglePiP();
      } catch (e) {
        alert("Picture-in-Picture não suportado: " + e);
      }
    });

    // Fechar stream
    btnClose.addEventListener("click", () => {
      this.stopWatchingPeer(streamId);
    });

    // Registra o stream ativo
    const activeStream: ActiveStream = {
      id: streamId,
      nick: displayHost,
      addr: displayAddr,
      player,
      element: tile,
      statBadge,
      volumeSlider,
      btnMute,
    };
    this.activeStreams.set(streamId, activeStream);

    this.streamsGrid.appendChild(tile);
    this.updateGridLayout();
    this.updateStatusPill();

    try {
      await invoke("watch_peer", { streamId, peerIp: streamId, onEvent });
    } catch (e) {
      alert(`Falha ao conectar em ${displayHost}: ` + e);
      this.stopWatchingPeer(streamId);
    }
  }

  /**
   * Encerra a visualização de um stream específico.
   */
  private async stopWatchingPeer(streamId: string) {
    const stream = this.activeStreams.get(streamId);
    if (stream) {
      stream.player.destroy();
      stream.element.remove();
      this.activeStreams.delete(streamId);
    }

    try {
      await invoke("stop_watching", { streamId });
    } catch (e) {
      console.warn("Erro ao desconectar stream:", e);
    }

    if (this.focusedStreamId === streamId) {
      this.clearStreamFocus();
    }

    this.updateGridLayout();
    this.updateStatusPill();

    // Se não houver mais nenhum stream e não estiver transmitindo, volta para a tela inicial
    if (this.activeStreams.size === 0 && !this.shareManager.getIsSharing()) {
      this.returnToSharePanel();
    }
  }

  /**
   * Desconecta de todas as transmissões assistidas.
   */
  private async stopAllWatching() {
    try {
      await invoke("stop_watching", { streamId: null });
    } catch (e) {
      console.warn("Erro ao parar todos os streams:", e);
    }

    for (const stream of this.activeStreams.values()) {
      stream.player.destroy();
      stream.element.remove();
    }
    this.activeStreams.clear();
    this.clearStreamFocus();

    if (this.isMiniPlayer) {
      this.isMiniPlayer = false;
      document.body.classList.remove("mini-player-active");
      try {
        const win = getCurrentWindow();
        await win.setAlwaysOnTop(false);
        await win.setSize(new LogicalSize(1100, 720));
      } catch (_) {}
    }

    this.updateGridLayout();
    this.updateStatusPill();

    if (!this.shareManager.getIsSharing()) {
      this.returnToSharePanel();
    }
  }

  private returnToSharePanel() {
    this.watchPanel.classList.add("hidden");
    this.sharePanel.classList.remove("hidden");
  }

  /**
   * Renderiza a miniatura ao vivo do próprio anfitrião na grade de chamadas.
   */
  private renderHostTile() {
    if (this.hostTileElement) return;

    const tile = document.createElement("div");
    tile.className = "stream-tile host-stream-tile";
    tile.id = "host-stream-tile";

    tile.innerHTML = `
      <div class="stream-tile-canvas-wrap">
        <canvas id="host-stream-tile-canvas" class="stream-tile-canvas"></canvas>
      </div>

      <div class="tile-top-overlay">
        <span class="badge-discord-live">🔴 LIVE</span>
        <span class="badge-tile-stat" id="host-tile-viewers">👥 Live Ativa</span>
      </div>

      <div class="tile-user-pill">
        <span class="tile-user-icon">🖥️</span>
        <span class="tile-user-name">Sua Transmissão (Você)</span>
      </div>

      <div class="tile-actions-overlay visible-always">
        <button class="btn-tile-action-pill" id="btn-host-tile-copy" title="Copiar código da sala P2P">📋 Copiar Código</button>
        <button class="btn-tile-action-pill" id="btn-host-tile-mute" title="Mutar/Desmutar som da sua transmissão">🔇 Mutar Som</button>
        <button class="btn-tile-action-pill danger" id="btn-host-tile-stop" title="Encerrar transmissão">⏹ Encerrar</button>
        <button class="btn-tile-icon btn-tile-focus" title="Focar sua live">⛶</button>
      </div>
    `;

    const canvas = tile.querySelector("#host-stream-tile-canvas") as HTMLCanvasElement;
    const btnCopy = tile.querySelector("#btn-host-tile-copy") as HTMLButtonElement;
    const btnMute = tile.querySelector("#btn-host-tile-mute") as HTMLButtonElement;
    const btnStop = tile.querySelector("#btn-host-tile-stop") as HTMLButtonElement;
    const btnFocus = tile.querySelector(".btn-tile-focus") as HTMLButtonElement;

    // StreamPlayer sem áudio para espelho local
    this.hostTilePlayer = new StreamPlayer(canvas, null, undefined, false);

    btnCopy.addEventListener("click", () => {
      const ticket = this.shareManager.getRoomTicket();
      if (!ticket) return;
      navigator.clipboard.writeText(ticket);
      btnCopy.textContent = "✅ Copiado!";
      setTimeout(() => (btnCopy.textContent = "📋 Copiar Código da Sala"), 2000);
    });

    btnMute.addEventListener("click", async () => {
      const muted = await this.shareManager.toggleAudioMute();
      btnMute.textContent = muted ? "🔊 Desmutar" : "🔇 Mutar Som";
    });

    btnStop.addEventListener("click", () => {
      this.shareManager.stopShare();
    });

    btnFocus.addEventListener("click", () => {
      this.toggleFocusStream("host");
    });

    this.hostTileElement = tile;
    this.streamsGrid.appendChild(tile);
    this.updateGridLayout();
  }

  private removeHostTile() {
    if (this.hostTilePlayer) {
      this.hostTilePlayer.destroy();
      this.hostTilePlayer = null;
    }
    if (this.hostTileElement) {
      this.hostTileElement.remove();
      this.hostTileElement = null;
    }
    if (this.focusedStreamId === "host") {
      this.clearStreamFocus();
    }
    this.updateGridLayout();
  }

  private toggleFocusStream(id: string) {
    if (this.focusedStreamId === id) {
      this.clearStreamFocus();
    } else {
      this.setStreamFocus(id);
    }
  }

  private setStreamFocus(id: string) {
    this.focusedStreamId = id;
    this.streamsGrid.classList.add("has-focused");
    this.btnCallGridMode.textContent = "⛶ Foco Ativo (Voltar à Grade)";

    // Remove foco anterior
    this.streamsGrid.querySelectorAll(".stream-tile").forEach((t) => t.classList.remove("focused"));

    if (id === "host") {
      if (this.hostTileElement) this.hostTileElement.classList.add("focused");
    } else {
      const stream = this.activeStreams.get(id);
      if (stream) stream.element.classList.add("focused");
    }
  }

  private clearStreamFocus() {
    this.focusedStreamId = null;
    this.streamsGrid.classList.remove("has-focused");
    this.streamsGrid.querySelectorAll(".stream-tile").forEach((t) => t.classList.remove("focused"));
    this.btnCallGridMode.textContent = "🪟 Grade";
  }

  /**
   * Atualiza a responsividade da grade de vídeos conforme o número de streams conectados.
   */
  private updateGridLayout() {
    const friendCount = this.activeStreams.size;
    const hasHost = this.hostTileElement !== null;
    const totalTiles = friendCount + (hasHost ? 1 : 0);

    // Atualiza badge de contagem
    if (totalTiles === 0) {
      this.streamsCountBadge.textContent = "0 transmissões";
    } else if (hasHost && friendCount > 0) {
      this.streamsCountBadge.textContent = `${friendCount} ${friendCount === 1 ? "amigo" : "amigos"} + sua live`;
    } else if (hasHost) {
      this.streamsCountBadge.textContent = "Sua live";
    } else {
      this.streamsCountBadge.textContent = `${friendCount} ${friendCount === 1 ? "stream ativo" : "streams ativos"}`;
    }

    // Configura classes de grade
    this.streamsGrid.classList.remove("count-1", "count-2", "count-3", "count-4", "count-many");
    if (totalTiles <= 1) {
      this.streamsGrid.classList.add("count-1");
    } else if (totalTiles === 2) {
      this.streamsGrid.classList.add("count-2");
    } else if (totalTiles <= 4) {
      this.streamsGrid.classList.add("count-4");
    } else {
      this.streamsGrid.classList.add("count-many");
    }
  }

  private escapeHtml(str: string): string {
    return str.replace(/[&<>'"]/g, (c) => `&#${c.charCodeAt(0)};`);
  }
}

// Inicializa a aplicação ao carregar o DOM
window.addEventListener("DOMContentLoaded", () => {
  new App();
});
