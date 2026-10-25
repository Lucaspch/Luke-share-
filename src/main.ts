import { invoke, Channel } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { ShareManager } from "./ui/share";
import {
  StreamPlayer,
  StreamEvent,
  StreamChannelMsg,
  extractBinaryBuffer,
} from "./media/player";
import { UpdaterManager } from "./ui/updater";
import { FriendsUI } from "./ui/friends";

interface MyInfo {
  id: string;
  nick: string;
  tag: string;
  status: "Idle" | "Streaming" | "Watching";
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
  private shareManager!: ShareManager;
  private updaterManager!: UpdaterManager;
  private friendsUI!: FriendsUI;
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
  private myStatusTextEl = document.getElementById("my-status-text") as HTMLElement | null;
  private nickInput = document.getElementById("nick-input") as HTMLInputElement;
  private btnSaveNick = document.getElementById("btn-save-nick") as HTMLButtonElement;
  private topTicketPill = document.getElementById("top-ticket-pill") as HTMLButtonElement | null;
  private topTicketPillLabel = document.getElementById("top-ticket-pill-label") as HTMLElement | null;
  private topViewersWrapper = document.getElementById("top-viewers-wrapper") as HTMLElement | null;

  // Painéis Principais do Palco
  private sharePanel = document.getElementById("share-panel") as HTMLElement;
  private watchPanel = document.getElementById("watch-panel") as HTMLElement;

  // Elementos da Chamada / Multi-Stream e Dock Inferior
  private streamsGridWrapper = document.getElementById("streams-grid-wrapper") as HTMLElement;
  private streamsGrid = document.getElementById("streams-grid") as HTMLElement;
  private streamsCountBadge = document.getElementById("streams-count-badge") as HTMLElement;
  private gridModeLabel = document.getElementById("grid-mode-label") as HTMLElement | null;
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
    this.friendsUI = new FriendsUI(
      this.myInfo?.tag || "",
      (roomTicket, hostNick) => {
        this.startWatchingPeer(roomTicket, hostNick);
      },
      (msg, icon) => this.showToast(msg, icon)
    );
    this.updaterManager = new UpdaterManager();
    this.bindEvents();
  }

  private showToast(message: string, iconName: string = "check_circle") {
    const container = document.getElementById("toast-container");
    if (!container) return;

    const toast = document.createElement("div");
    toast.className = "toast-item";
    toast.innerHTML = `
      <span class="material-symbols-rounded ms-sm">${iconName}</span>
      <span>${this.escapeHtml(message)}</span>
    `;
    container.appendChild(toast);

    setTimeout(() => {
      toast.style.opacity = "0";
      toast.style.transform = "translateY(8px)";
      toast.style.transition = "all 0.2s ease";
      setTimeout(() => toast.remove(), 220);
    }, 2800);
  }

  private async fetchMyInfo() {
    try {
      const savedNick = localStorage.getItem("luke_user_nick");
      const info = await invoke<MyInfo>("get_my_info");
      this.myInfo = info;
      if (savedNick && savedNick.trim() && info.nick !== savedNick.trim()) {
        await invoke("set_my_nick", { nick: savedNick.trim() }).catch(() => {});
        this.myInfo.nick = savedNick.trim();
      }
      this.updateMyInfoUI();
    } catch (e) {
      console.warn("Erro ao obter informações locais:", e);
    }
  }

  private updateMyInfoUI() {
    if (!this.myInfo) return;

    this.myIpEl.textContent = "P2P Nativo Pronto";
    this.myIpEl.title = "Iroh P2P Nativo + Áudio WASAPI + IPC Binário";

    this.nickInput.value = this.myInfo.nick;
    if (this.myInfo.tag && this.friendsUI) {
      this.friendsUI.setMyTag(this.myInfo.tag);
    }
    this.updateStatusPill();
  }

  private updateStatusPill() {
    const isSharing = this.shareManager ? this.shareManager.getIsSharing() : false;
    const watchCount = this.activeStreams.size;

    this.myStatusEl.className = "status-pill";
    const setStatusText = (txt: string) => {
      if (this.myStatusTextEl) {
        this.myStatusTextEl.textContent = txt;
      } else {
        this.myStatusEl.textContent = txt;
      }
    };

    if (isSharing && watchCount > 0) {
      this.myStatusEl.classList.add("streaming");
      setStatusText(`Transmitindo + Assistindo (${watchCount})`);
    } else if (isSharing) {
      this.myStatusEl.classList.add("streaming");
      setStatusText("Transmitindo");
    } else if (watchCount > 0) {
      this.myStatusEl.classList.add("watching");
      setStatusText(`Assistindo (${watchCount})`);
    } else {
      setStatusText("Ocioso");
    }
  }

  private isValidRoomCode(raw: string): boolean {
    const trimmed = raw.trim();
    if (trimmed.startsWith("lk-") && trimmed.length > 16) {
      return true;
    }
    const clean = trimmed.toUpperCase().replace(/[^A-Z0-9]/g, "");
    const body = clean.startsWith("LK") && clean.length === 10 ? clean.slice(2) : clean;
    return /^[2-9A-HJ-NP-Z]{8}$/.test(body);
  }

  private initPeers() {
    const manualIpInput = document.getElementById("manual-ip-input") as HTMLInputElement;
    const btnAddManual = document.getElementById("btn-add-manual") as HTMLButtonElement;

    const handleConnectRoom = () => {
      const val = manualIpInput.value.trim();
      if (!val) return;
      if (/^LK#[2-9A-HJ-NP-Z]{4}$/i.test(val)) {
        this.showToast(
          "Isso é uma Tag de Amigo (LK#XXXX)! Clique em 'Amigos' no topo direito para adicioná-lo.",
          "group_add"
        );
        return;
      }
      if (!this.isValidRoomCode(val)) {
        this.showToast(
          "Código inválido. Digite um código curto (LK-XXXX-XXXX) ou cole o ticket (lk-...).",
          "error"
        );
        return;
      }
      manualIpInput.value = "";
      const label = val.startsWith("lk-") ? "Sala P2P" : val.toUpperCase();
      this.startWatchingPeer(val, label);
    };

    btnAddManual.addEventListener("click", handleConnectRoom);
    manualIpInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleConnectRoom();
    });
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
        audioModeGroup: document.getElementById("audio-mode-group"),
        audioModeSelect: document.getElementById("audio-mode-select") as HTMLSelectElement,
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
          audioModeGroup: document.getElementById("modal-audio-mode-group"),
          audioModeSelect: document.getElementById("modal-audio-mode-select") as HTMLSelectElement,
        },
      },
      (streaming) => {
        if (streaming) {
          this.btnCallShare.classList.add("active-share");
          this.btnCallShare.innerHTML = '<span class="material-symbols-rounded">cast_connected</span>';
          this.btnCallShare.title = "Transmitindo ao vivo (Clique para gerenciar ou encerrar sua tela)";

          const ticket = this.shareManager.getRoomTicket();
          const shortCode = this.shareManager.getShortCode();
          if (this.topTicketPill && ticket) {
            this.topTicketPill.classList.remove("hidden");
            if (this.topTicketPillLabel) {
              this.topTicketPillLabel.textContent = shortCode || "Código";
            }
            this.topTicketPill.title = `Copiar Código: ${ticket} (Shift+Clique copia ticket lk-... completo)`;
          }
          if (this.topViewersWrapper) {
            this.topViewersWrapper.classList.remove("hidden");
            this.updateViewersBadge(this.topViewersWrapper, 0, []);
          }

          // Entra imediatamente no Palco de Transmissão (Grade 1 / 2 / 4 Streams)
          this.sharePanel.classList.add("hidden");
          this.watchPanel.classList.remove("hidden");
          this.renderHostTile();
        } else {
          this.btnCallShare.classList.remove("active-share");
          this.btnCallShare.innerHTML = '<span class="material-symbols-rounded">screen_share</span>';
          this.btnCallShare.title = "Compartilhar sua tela";
          if (this.topTicketPill) {
            this.topTicketPill.classList.add("hidden");
            if (this.topTicketPillLabel) {
              this.topTicketPillLabel.textContent = "Código";
            }
          }
          if (this.topViewersWrapper) {
            this.topViewersWrapper.classList.add("hidden");
            this.updateViewersBadge(this.topViewersWrapper, 0, []);
          }
          this.removeHostTile();
        }
        this.updateStatusPill();
        this.updateGridLayout();
      }
    );

    // Atualiza contagem de espectadores e lista de quem está assistindo (Host e barra superior)
    this.shareManager.onViewersChange = (count, viewers, _text) => {
      const hostWrapper = document.getElementById("host-viewers-wrapper") as HTMLElement | null;
      this.updateViewersBadge(hostWrapper, count, viewers);

      if (this.topViewersWrapper) {
        this.updateViewersBadge(this.topViewersWrapper, count, viewers);
      }
    };

    // Encaminha pacotes binários diretos (IPC Binário sem Base64) para o host tile quando ativo na grade
    this.shareManager.onPreviewBinaryFrame = (buf: ArrayBuffer) => {
      if (this.hostTilePlayer) {
        this.hostTilePlayer.handleBinaryVideoPacket(buf);
      }
    };

    this.shareManager.onPreviewStreamEvent = (msg: StreamEvent) => {
      if (!this.hostTilePlayer) return;
      if (msg.t === "config") {
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
        localStorage.setItem("luke_user_nick", nick);
        await invoke("set_my_nick", { nick });
        if (this.myInfo) this.myInfo.nick = nick;
        this.showToast(`Apelido atualizado para "${nick}"`, "check_circle");
      } catch (e) {
        alert("Erro ao salvar apelido: " + e);
      }
    };
    this.btnSaveNick.addEventListener("click", handleSaveNick);
    this.nickInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleSaveNick();
    });
    this.nickInput.addEventListener("blur", () => {
      const nick = this.nickInput.value.trim();
      if (nick && (!this.myInfo || this.myInfo.nick !== nick)) {
        handleSaveNick();
      }
    });

    // Logo Home Click
    const btnHomeLogo = document.getElementById("btn-home-logo");
    if (btnHomeLogo) {
      btnHomeLogo.addEventListener("click", (e) => {
        e.preventDefault();
        if (this.activeStreams.size === 0 && !this.shareManager.getIsSharing()) {
          this.returnToSharePanel();
        }
      });
    }

    // Círculo 1 do Dock: Compartilhar Tela / Alternar Fonte (Seamless Switch)
    this.btnCallShare.addEventListener("click", () => {
      if (this.shareManager.getIsSharing()) {
        // Se já estiver transmitindo, abre o modal para alternar a tela/janela sem derrubar a sala
        this.shareManager.openShareModal(true);
      } else if (!this.watchPanel.classList.contains("hidden")) {
        // Se já estiver assistindo amigos no palco, abre o modal para escolher a tela
        this.shareManager.openShareModal(false);
      } else {
        // Se estiver no Estúdio inicial, inicia a transmissão da fonte selecionada imediatamente
        this.shareManager.startShare(false);
      }
    });

    // Copiar Código da Sala (Dock ou Pílula do Topo; Shift+Clique copia o ticket completo lk-...)
    const copyRoomTicketAction = (ev?: MouseEvent) => {
      const useRaw = ev?.shiftKey && this.shareManager.getRawRoomTicket();
      const ticket = useRaw
        ? this.shareManager.getRawRoomTicket()
        : this.shareManager.getRoomTicket();
      if (ticket) {
        navigator.clipboard.writeText(ticket);
        const label = ticket.startsWith("LK-")
          ? `Código curto ${ticket} copiado!`
          : "Ticket P2P completo copiado!";
        this.showToast(label, "content_copy");
      } else {
        this.showToast("Inicie uma transmissão para gerar seu Código Curto (LK-XXXX-XXXX)", "info");
      }
    };

    const btnCallCopyTicket = document.getElementById("btn-call-copy-ticket") as HTMLButtonElement | null;
    if (btnCallCopyTicket) {
      btnCallCopyTicket.addEventListener("click", (ev) => copyRoomTicketAction(ev));
    }
    if (this.topTicketPill) {
      this.topTicketPill.addEventListener("click", (ev) => copyRoomTicketAction(ev));
    }

    // Círculo 2 do Dock: Mutar/Desmutar Áudio do Sistema
    const btnCallMuteMic = document.getElementById("btn-call-mute-mic") as HTMLButtonElement | null;
    const callMicIcon = document.getElementById("call-mic-icon") as HTMLElement | null;
    if (btnCallMuteMic) {
      btnCallMuteMic.addEventListener("click", async () => {
        const muted = await this.shareManager.toggleAudioMute();
        if (callMicIcon) {
          callMicIcon.textContent = muted ? "volume_off" : "volume_up";
        }
        btnCallMuteMic.title = muted ? "Desmutar áudio da transmissão" : "Mutar áudio da transmissão";
        btnCallMuteMic.classList.toggle("btn-call-muted", muted);

        const hostMuteBtn = document.getElementById("btn-host-tile-mute");
        if (hostMuteBtn) {
          hostMuteBtn.innerHTML = muted
            ? '<span class="material-symbols-rounded ms-xs">volume_up</span><span>Desmutar</span>'
            : '<span class="material-symbols-rounded ms-xs">volume_off</span><span>Mutar Som</span>';
        }

        this.showToast(
          muted ? "Áudio do sistema mutado na transmissão" : "Áudio do sistema ativado na transmissão",
          muted ? "volume_off" : "volume_up"
        );
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

    // Círculo 3 do Dock: Desconectar / Sair da Sala
    this.btnStopAllWatch.addEventListener("click", () => {
      if (this.activeStreams.size === 0 && !this.shareManager.getIsSharing()) {
        this.shareManager.refreshSources();
        this.showToast("Fontes de captura atualizadas", "refresh");
        return;
      }
      this.stopAllWatching();
      this.showToast("Você saiu da sala de transmissão", "call_end");
    });

    // Modo Mini-Player (Compacto & Always on Top)
    const toggleMiniPlayer = async () => {
      try {
        const win = getCurrentWindow();
        this.isMiniPlayer = !this.isMiniPlayer;
        if (this.isMiniPlayer) {
          document.body.classList.add("mini-player-active");
          await win.setAlwaysOnTop(true);
          await win.setSize(new LogicalSize(480, 290));
        } else {
          document.body.classList.remove("mini-player-active");
          await win.setAlwaysOnTop(false);
          await win.setSize(new LogicalSize(1100, 720));
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
      const targetEl = !this.watchPanel.classList.contains("hidden")
        ? this.streamsGridWrapper
        : document.documentElement;
      if (!document.fullscreenElement) {
        targetEl.requestFullscreen().catch((err) => {
          console.warn("Fullscreen falhou:", err);
        });
      } else {
        document.exitFullscreen();
      }
    });
  }

  /**
   * Conecta a um amigo (ou código de sala lk-...) e adiciona uma tile à Grade da Sala.
   * Mantém sua própria transmissão ativa na grade simultaneamente!
   */
  private async startWatchingPeer(ip: string, nick: string) {
    const streamId = ip.trim();

    if (this.activeStreams.has(streamId)) {
      this.setStreamFocus(streamId);
      return;
    }

    const displayHost = nick === "Sala P2P" ? "Amigo (P2P)" : nick;
    const displayAddr = "P2P Internet";

    this.sharePanel.classList.add("hidden");
    this.watchPanel.classList.remove("hidden");

    if (this.shareManager.getIsSharing()) {
      this.renderHostTile();
    }

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
        <div class="tile-reconnecting-banner hidden">
          <span class="reconnecting-dot"></span>
          <span class="reconnecting-text">Reconectando...</span>
        </div>
      </div>

      <div class="tile-top-overlay">
        <span class="badge-discord-live">
          <span class="material-symbols-rounded ms-xs">sensors</span>
          <span>LIVE</span>
        </span>
        <span class="badge-tile-stat tile-stat-badge">Conectando...</span>
        <div class="viewers-badge-wrapper tile-viewers-wrapper">
          <div class="badge-tile-stat badge-viewers-pill" title="Passe o mouse para ver quem está assistindo">
            <span class="material-symbols-rounded ms-xs">group</span>
            <span class="viewers-count-num">0</span>
          </div>
          <div class="viewers-tooltip">
            <div class="viewers-tooltip-header">
              <span class="material-symbols-rounded ms-xs">visibility</span>
              <span>Assistindo (<strong class="tooltip-count">0</strong>)</span>
            </div>
            <div class="viewers-tooltip-list">
              <div class="viewers-empty-state">
                <span class="material-symbols-rounded ms-xs">person_off</span>
                <span>Nenhum espectador ainda</span>
              </div>
            </div>
          </div>
        </div>
      </div>

      <div class="tile-user-pill">
        <span class="material-symbols-rounded">desktop_windows</span>
        <span class="tile-user-name" title="${this.escapeHtml(displayHost)}">${this.escapeHtml(displayHost)}</span>
      </div>

      <div class="tile-actions-overlay">
        <div class="tile-volume-popover">
          <button class="btn-tile-icon btn-tile-mute" title="Mutar áudio deste amigo">
            <span class="material-symbols-rounded ms-sm">volume_up</span>
          </button>
          <input type="range" class="tile-volume-slider" min="0" max="1" step="0.05" value="1" title="Volume individual" />
        </div>
        <button class="btn-tile-icon btn-tile-pip" title="Picture-in-Picture">
          <span class="material-symbols-rounded ms-sm">picture_in_picture_alt</span>
        </button>
        <button class="btn-tile-icon btn-tile-focus" title="Focar/Expandir este stream (ou dê duplo clique)">
          <span class="material-symbols-rounded ms-sm">open_in_full</span>
        </button>
        <button class="btn-tile-icon btn-tile-close" title="Desconectar desta transmissão">
          <span class="material-symbols-rounded ms-sm">close</span>
        </button>
      </div>
    `;

    const canvas = tile.querySelector("canvas") as HTMLCanvasElement;
    const overlay = tile.querySelector(".player-overlay") as HTMLElement;
    const reconnectingBanner = tile.querySelector(".tile-reconnecting-banner") as HTMLElement;
    const reconnectingText = tile.querySelector(".reconnecting-text") as HTMLElement;
    const statBadge = tile.querySelector(".tile-stat-badge") as HTMLElement;
    const volumeSlider = tile.querySelector(".tile-volume-slider") as HTMLInputElement;
    const btnMute = tile.querySelector(".btn-tile-mute") as HTMLButtonElement;
    const btnPip = tile.querySelector(".btn-tile-pip") as HTMLButtonElement;
    const btnFocus = tile.querySelector(".btn-tile-focus") as HTMLButtonElement;
    const btnClose = tile.querySelector(".btn-tile-close") as HTMLButtonElement;

    let currentRes = "1080p";
    let currentFps = "60 FPS";
    let currentCodec = "H.264";

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

    const onEvent = new Channel<StreamChannelMsg>();
    onEvent.onmessage = (msg: StreamChannelMsg) => {
      const bin = extractBinaryBuffer(msg);
      if (bin) {
        player.handleBinaryVideoPacket(bin);
        return;
      }

      const ev = msg as StreamEvent;
      switch (ev.t) {
        case "config":
          if (ev.width && ev.height) {
            player.setDimensions(ev.width, ev.height);
            currentRes = `${ev.width}x${ev.height}`;
          }
          if (ev.fps) {
            currentFps = `${ev.fps} FPS`;
          }
          if (ev.codec) {
            player.configureDecoder(ev.codec);
            currentCodec = ev.codec.toUpperCase();
          }
          statBadge.textContent = `${currentRes} • ${currentFps} • ${currentCodec}`;
          break;
        case "reconnecting":
          reconnectingBanner.classList.remove("hidden");
          reconnectingText.textContent = `Reconectando... (${ev.remaining_secs}s)`;
          break;
        case "reconnected":
          reconnectingBanner.classList.add("hidden");
          player.markAwaitingKeyFrame();
          this.showToast("Conexão restaurada automaticamente!", "wifi");
          break;
        case "viewers": {
          const wrapper = tile.querySelector(".tile-viewers-wrapper") as HTMLElement | null;
          this.updateViewersBadge(wrapper, ev.count, ev.viewers);
          break;
        }
        case "telemetry": {
          if (ev.packets_rx > 0 && !reconnectingBanner.classList.contains("hidden")) {
            reconnectingBanner.classList.add("hidden");
          }
          currentFps = `${ev.fps} FPS`;
          const relayInfo = ev.via_relay ? " • Mesh P2P" : "";
          const fecInfo = ev.fec_recovered ? ` • FEC:${ev.fec_recovered}` : "";
          const lossInfo = ev.loss_pct > 1 ? ` • ${ev.loss_pct.toFixed(1)}% perda` : "";
          statBadge.textContent = `${currentRes} • ${currentFps} • ${ev.rtt_ms}ms${relayInfo}${fecInfo}${lossInfo}`;
          break;
        }
        case "ended":
          this.stopWatchingPeer(streamId);
          break;
      }
    };

    // Controle de volume e mudo direto no motor WASAPI nativo em Rust
    volumeSlider.addEventListener("input", () => {
      const vol = parseFloat(volumeSlider.value);
      invoke("set_stream_volume", { streamId, volume: vol }).catch(() => {});
      invoke("set_stream_muted", { streamId, muted: vol === 0 }).catch(() => {});
      btnMute.innerHTML = `<span class="material-symbols-rounded ms-sm">${
        vol === 0 ? "volume_off" : "volume_up"
      }</span>`;
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

    btnFocus.addEventListener("click", () => {
      this.toggleFocusStream(streamId);
    });

    // Duplo clique no vídeo para expandir/restaurar na grade
    canvas.addEventListener("dblclick", () => {
      this.toggleFocusStream(streamId);
    });

    btnPip.addEventListener("click", async () => {
      try {
        await player.togglePiP();
      } catch (e) {
        alert("Picture-in-Picture não suportado: " + e);
      }
    });

    btnClose.addEventListener("click", () => {
      this.stopWatchingPeer(streamId);
    });

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

    if (this.activeStreams.size === 0 && !this.shareManager.getIsSharing()) {
      this.returnToSharePanel();
    }
  }

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

    if (this.shareManager.getIsSharing()) {
      await this.shareManager.stopShare();
    }

    this.updateGridLayout();
    this.updateStatusPill();
    this.returnToSharePanel();
  }

  private returnToSharePanel() {
    this.watchPanel.classList.add("hidden");
    this.sharePanel.classList.remove("hidden");
  }

  /**
   * Renderiza a tile ao vivo do próprio anfitrião na grade da sala.
   */
  private renderHostTile() {
    if (this.hostTileElement) return;

    const tile = document.createElement("div");
    tile.className = "stream-tile host-stream-tile";
    tile.id = "host-stream-tile";

    tile.innerHTML = `
      <div class="stream-tile-canvas-wrap">
        <canvas id="host-stream-tile-canvas" class="stream-tile-canvas"></canvas>
        <div id="host-tile-overlay" class="player-overlay">
          <div class="spinner"></div>
          <div class="overlay-text">Iniciando prévia da sua transmissão...</div>
        </div>
      </div>

      <div class="tile-top-overlay">
        <span class="badge-discord-live">
          <span class="material-symbols-rounded ms-xs">sensors</span>
          <span>LIVE</span>
        </span>
        <div class="viewers-badge-wrapper" id="host-viewers-wrapper">
          <div class="badge-tile-stat badge-viewers-pill" id="host-viewers-pill" title="Passe o mouse para ver quem está assistindo">
            <span class="material-symbols-rounded ms-xs">group</span>
            <span class="viewers-count-num" id="host-viewers-count">0</span>
          </div>
          <div class="viewers-tooltip" id="host-viewers-tooltip">
            <div class="viewers-tooltip-header">
              <span class="material-symbols-rounded ms-xs">visibility</span>
              <span>Assistindo sua tela (<strong class="tooltip-count" id="host-tooltip-count">0</strong>)</span>
            </div>
            <div class="viewers-tooltip-list" id="host-viewers-list">
              <div class="viewers-empty-state">
                <span class="material-symbols-rounded ms-xs">person_off</span>
                <span>Nenhum espectador ainda</span>
              </div>
            </div>
          </div>
        </div>
      </div>

      <div class="tile-user-pill">
        <span class="material-symbols-rounded">desktop_windows</span>
        <span class="tile-user-name">Sua Transmissão (Você)</span>
      </div>

      <div class="tile-actions-overlay visible-always">
        <button class="btn-tile-action-pill" id="btn-host-tile-copy" title="Copiar código da sala P2P">
          <span class="material-symbols-rounded ms-xs">content_copy</span>
          <span>Copiar Código</span>
        </button>
        <button class="btn-tile-action-pill" id="btn-host-tile-mute" title="Mutar/Desmutar som da sua transmissão">
          <span class="material-symbols-rounded ms-xs">volume_off</span>
          <span>Mutar Som</span>
        </button>
        <button class="btn-tile-action-pill danger" id="btn-host-tile-stop" title="Encerrar transmissão">
          <span class="material-symbols-rounded ms-xs">stop_circle</span>
          <span>Encerrar</span>
        </button>
        <button class="btn-tile-icon btn-tile-focus" title="Focar sua live (ou dê duplo clique)">
          <span class="material-symbols-rounded ms-sm">open_in_full</span>
        </button>
      </div>
    `;

    const canvas = tile.querySelector("#host-stream-tile-canvas") as HTMLCanvasElement;
    const overlay = tile.querySelector("#host-tile-overlay") as HTMLElement;
    const btnCopy = tile.querySelector("#btn-host-tile-copy") as HTMLButtonElement;
    const btnMute = tile.querySelector("#btn-host-tile-mute") as HTMLButtonElement;
    const btnStop = tile.querySelector("#btn-host-tile-stop") as HTMLButtonElement;
    const btnFocus = tile.querySelector(".btn-tile-focus") as HTMLButtonElement;

    this.hostTilePlayer = new StreamPlayer(canvas, overlay, undefined, false);

    const shareInfo = this.shareManager.getShareInfo();
    if (shareInfo) {
      if (shareInfo.width && shareInfo.height) {
        this.hostTilePlayer.setDimensions(shareInfo.width, shareInfo.height);
      }
      if (shareInfo.codec) {
        this.hostTilePlayer.configureDecoder(shareInfo.codec);
      }
    }

    btnCopy.addEventListener("click", (ev) => {
      const useRaw = ev.shiftKey && this.shareManager.getRawRoomTicket();
      const ticket = useRaw
        ? this.shareManager.getRawRoomTicket()
        : this.shareManager.getRoomTicket();
      if (!ticket) return;
      navigator.clipboard.writeText(ticket);
      const label = ticket.startsWith("LK-")
        ? `Código curto ${ticket} copiado!`
        : "Ticket P2P completo copiado!";
      this.showToast(label, "content_copy");
    });

    btnMute.addEventListener("click", async () => {
      const muted = await this.shareManager.toggleAudioMute();
      btnMute.innerHTML = muted
        ? '<span class="material-symbols-rounded ms-xs">volume_up</span><span>Desmutar</span>'
        : '<span class="material-symbols-rounded ms-xs">volume_off</span><span>Mutar Som</span>';
      const callMicIcon = document.getElementById("call-mic-icon");
      const btnCallMuteMic = document.getElementById("btn-call-mute-mic");
      if (callMicIcon) callMicIcon.textContent = muted ? "volume_off" : "volume_up";
      if (btnCallMuteMic) btnCallMuteMic.classList.toggle("btn-call-muted", muted);
    });

    btnStop.addEventListener("click", () => {
      this.shareManager.stopShare();
      this.showToast("Transmissão encerrada", "stop_circle");
    });

    btnFocus.addEventListener("click", () => {
      this.toggleFocusStream("host");
    });

    canvas.addEventListener("dblclick", () => {
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

    if (this.activeStreams.size === 0) {
      this.returnToSharePanel();
    }
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
    if (this.gridModeLabel) {
      this.gridModeLabel.textContent = "Voltar à Grade";
    }

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
    if (this.gridModeLabel) {
      this.gridModeLabel.textContent = "Grade";
    }
  }

  private updateGridLayout() {
    const friendCount = this.activeStreams.size;
    const hasHost = this.hostTileElement !== null;
    const totalTiles = friendCount + (hasHost ? 1 : 0);

    if (totalTiles === 0) {
      this.streamsCountBadge.textContent = "Estúdio Pronto";
    } else if (hasHost && friendCount > 0) {
      this.streamsCountBadge.textContent = `${friendCount} ${
        friendCount === 1 ? "amigo" : "amigos"
      } + sua live`;
    } else if (hasHost) {
      this.streamsCountBadge.textContent = "Sua live ativa";
    } else {
      this.streamsCountBadge.textContent = `${friendCount} ${
        friendCount === 1 ? "stream ativo" : "streams ativos"
      }`;
    }

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

  private updateViewersBadge(wrapper: HTMLElement | null, count: number, viewers: string[]) {
    if (!wrapper) return;
    const countEl = wrapper.querySelector(".viewers-count-num");
    const tooltipCountEl = wrapper.querySelector(".tooltip-count");
    const listEl = wrapper.querySelector(".viewers-tooltip-list");

    if (countEl) countEl.textContent = `${count}`;
    if (tooltipCountEl) tooltipCountEl.textContent = `${count}`;

    if (listEl) {
      if (!viewers || viewers.length === 0) {
        listEl.innerHTML = `
          <div class="viewers-empty-state">
            <span class="material-symbols-rounded ms-xs">person_off</span>
            <span>Nenhum espectador ainda</span>
          </div>
        `;
      } else {
        listEl.innerHTML = viewers
          .map(
            (name) => `
          <div class="viewer-entry">
            <span class="material-symbols-rounded ms-xs">person</span>
            <span class="viewer-name">${this.escapeHtml(name)}</span>
          </div>
        `
          )
          .join("");
      }
    }
  }

  private escapeHtml(str: string): string {
    return str.replace(/[&<>'"]/g, (c) => `&#${c.charCodeAt(0)};`);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  new App();
});
