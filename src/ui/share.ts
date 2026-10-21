import { invoke, Channel } from "@tauri-apps/api/core";
import { StreamPlayer, StreamEvent } from "../media/player";

export type SourceInfo =
  | { kind: "Monitor"; id: number; name: string; width: number; height: number; primary: boolean; refresh_rate: number }
  | { kind: "Window"; id: number; title: string; pid: number; exe: string; refresh_rate: number };

export interface ShareInfo {
  width: number;
  height: number;
  fps: number;
  codec?: string;
  media_port: number;
  room_ticket?: string | null;
}

export interface StreamStats {
  is_sharing: boolean;
  viewers_count: number;
  viewers: string[];
  packets_sent: number;
  bytes_sent: number;
  nacks_served: number;
}

export interface ModalShareElements {
  modalBackdrop: HTMLElement;
  btnClose: HTMLButtonElement;
  btnCancel: HTMLButtonElement;
  btnStart: HTMLButtonElement;
  tabButtons: NodeListOf<HTMLButtonElement>;
  sourcesGrid: HTMLElement;
  sourcePreviewImg: HTMLImageElement;
  sourcePreviewPlaceholder: HTMLElement;
  btnRefreshPreview: HTMLButtonElement;
  codecSelect: HTMLSelectElement;
  resolutionSelect: HTMLSelectElement;
  fpsSelect: HTMLSelectElement;
  bitrateSelect: HTMLSelectElement;
  audioCheckbox: HTMLInputElement;
}

export interface ShareManagerElements {
  sourcesGrid: HTMLElement;
  btnRefresh: HTMLButtonElement;
  tabButtons: NodeListOf<HTMLButtonElement>;
  resolutionSelect: HTMLSelectElement;
  fpsSelect?: HTMLSelectElement;
  bitrateSelect?: HTMLSelectElement;
  codecSelect?: HTMLSelectElement;
  sourcePreviewImg?: HTMLImageElement;
  sourcePreviewPlaceholder?: HTMLElement;
  btnRefreshPreview?: HTMLButtonElement;
  audioCheckbox: HTMLInputElement;
  btnStart: HTMLButtonElement;
  btnStop: HTMLButtonElement;
  btnStopTop: HTMLButtonElement;
  sourcesSelectionArea: HTMLElement;
  hostPreviewCard: HTMLElement;
  hostPreviewCanvas: HTMLCanvasElement;
  hostPreviewOverlay: HTMLElement;
  previewFpsTag: HTMLElement;
  previewResTag: HTMLElement;
  previewViewersCount: HTMLElement;
  statPort: HTMLElement;
  statAudioTag: HTMLElement;
  btnToggleMute: HTMLButtonElement;
  roomTicketBox?: HTMLElement;
  shareTicketInput?: HTMLInputElement;
  btnCopyTicket?: HTMLButtonElement;
  modal?: ModalShareElements;
}

export class ShareManager {
  private sourcesGrid: HTMLElement;
  private btnRefresh: HTMLButtonElement;
  private tabButtons: NodeListOf<HTMLButtonElement>;
  private resolutionSelect: HTMLSelectElement;
  private fpsSelect: HTMLSelectElement | null = null;
  private bitrateSelect: HTMLSelectElement | null = null;
  private audioCheckbox: HTMLInputElement;
  private btnStart: HTMLButtonElement;
  private btnStop: HTMLButtonElement;
  private btnStopTop: HTMLButtonElement;
  sourcesSelectionArea: HTMLElement;
  private hostPreviewCard: HTMLElement;
  private hostPreviewCanvas: HTMLCanvasElement;
  private hostPreviewOverlay: HTMLElement;
  private previewFpsTag: HTMLElement;
  private previewResTag: HTMLElement;
  private previewViewersCount: HTMLElement;
  private statPort: HTMLElement;
  private statAudioTag: HTMLElement;
  private btnToggleMute: HTMLButtonElement;
  private roomTicketBox: HTMLElement | null = null;
  private shareTicketInput: HTMLInputElement | null = null;
  private btnCopyTicket: HTMLButtonElement | null = null;
  private codecSelect: HTMLSelectElement | null = null;
  private sourcePreviewImg: HTMLImageElement | null = null;
  private sourcePreviewPlaceholder: HTMLElement | null = null;
  private btnRefreshPreview: HTMLButtonElement | null = null;
  private currentRoomTicket: string | null = null;
  private modal?: ModalShareElements;
  public onPreviewStreamEvent?: (msg: StreamEvent) => void;
  public onViewersCountChange?: (count: number, text: string) => void;
  public onViewersChange?: (count: number, viewers: string[], text: string) => void;
  private currentShareInfo: ShareInfo | null = null;

  private currentSources: SourceInfo[] = [];
  private activeTab: "monitors" | "windows" = "monitors";
  private selectedSourceId: number | null = null;
  private isSharing = false;
  private isAudioMuted = false;
  private previewPlayer: StreamPlayer | null = null;
  private statsInterval: number | null = null;
  private onStatusChange?: (streaming: boolean) => void;

  constructor(
    elements: ShareManagerElements,
    onStatusChange?: (streaming: boolean) => void
  ) {
    this.sourcesGrid = elements.sourcesGrid;
    this.btnRefresh = elements.btnRefresh;
    this.tabButtons = elements.tabButtons;
    this.resolutionSelect = elements.resolutionSelect;
    this.fpsSelect = elements.fpsSelect || null;
    this.bitrateSelect = elements.bitrateSelect || null;
    this.codecSelect = elements.codecSelect || null;
    this.sourcePreviewImg = elements.sourcePreviewImg || null;
    this.sourcePreviewPlaceholder = elements.sourcePreviewPlaceholder || null;
    this.btnRefreshPreview = elements.btnRefreshPreview || null;
    this.audioCheckbox = elements.audioCheckbox;
    this.btnStart = elements.btnStart;
    this.btnStop = elements.btnStop;
    this.btnStopTop = elements.btnStopTop;
    this.sourcesSelectionArea = elements.sourcesSelectionArea;
    this.hostPreviewCard = elements.hostPreviewCard;
    this.hostPreviewCanvas = elements.hostPreviewCanvas;
    this.hostPreviewOverlay = elements.hostPreviewOverlay;
    this.previewFpsTag = elements.previewFpsTag;
    this.previewResTag = elements.previewResTag;
    this.previewViewersCount = elements.previewViewersCount;
    this.statPort = elements.statPort;
    this.statAudioTag = elements.statAudioTag;
    this.btnToggleMute = elements.btnToggleMute;
    this.roomTicketBox = elements.roomTicketBox || null;
    this.shareTicketInput = elements.shareTicketInput || null;
    this.btnCopyTicket = elements.btnCopyTicket || null;
    this.modal = elements.modal;
    this.onStatusChange = onStatusChange;

    this.bindEvents();
    this.refreshSources();
  }

  private bindEvents() {
    this.btnRefresh.addEventListener("click", () => this.refreshSources());

    this.tabButtons.forEach((tab) => {
      tab.addEventListener("click", () => {
        this.tabButtons.forEach((t) => t.classList.remove("active"));
        tab.classList.add("active");
        this.activeTab = tab.dataset.tab as "monitors" | "windows";
        this.renderSources();
      });
    });

    this.btnStart.addEventListener("click", () => this.startShare());
    this.btnStop.addEventListener("click", () => this.stopShare());
    if (this.btnStopTop) {
      this.btnStopTop.addEventListener("click", () => this.stopShare());
    }

    this.btnToggleMute.addEventListener("click", async () => {
      this.isAudioMuted = !this.isAudioMuted;
      try {
        await invoke("set_audio_muted", { muted: this.isAudioMuted });
        this.updateAudioTag();
      } catch (e) {
        console.warn("Erro ao alternar mudo:", e);
      }
    });

    if (this.btnCopyTicket && this.shareTicketInput) {
      this.btnCopyTicket.addEventListener("click", () => {
        if (!this.currentRoomTicket) return;
        navigator.clipboard.writeText(this.currentRoomTicket);
        const originalText = this.btnCopyTicket!.textContent;
        this.btnCopyTicket!.textContent = "Copiado!";
        setTimeout(() => {
          if (this.btnCopyTicket) {
            this.btnCopyTicket.textContent = originalText;
          }
        }, 2000);
      });
    }

    if (this.btnRefreshPreview) {
      this.btnRefreshPreview.addEventListener("click", () => {
        if (this.selectedSourceId !== null) {
          this.updateSourceSnapshot(this.selectedSourceId);
        }
      });
    }

    if (this.modal) {
      this.modal.btnClose.addEventListener("click", () => this.closeShareModal());
      this.modal.btnCancel.addEventListener("click", () => this.closeShareModal());
      this.modal.btnStart.addEventListener("click", () => this.startShare(true));
      this.modal.btnRefreshPreview.addEventListener("click", () => {
        if (this.selectedSourceId !== null) {
          this.updateSourceSnapshot(this.selectedSourceId);
        }
      });
      this.modal.tabButtons.forEach((tab) => {
        tab.addEventListener("click", () => {
          this.tabButtons.forEach((t) => t.classList.remove("active"));
          this.modal?.tabButtons.forEach((t) => t.classList.remove("active"));
          tab.classList.add("active");
          this.activeTab = tab.dataset.tab as "monitors" | "windows";
          this.renderSources();
        });
      });
    }
  }

  public openShareModal() {
    if (!this.modal) return;
    this.modal.resolutionSelect.value = this.resolutionSelect.value;
    if (this.fpsSelect && this.modal.fpsSelect) this.modal.fpsSelect.value = this.fpsSelect.value;
    if (this.bitrateSelect && this.modal.bitrateSelect) this.modal.bitrateSelect.value = this.bitrateSelect.value;
    if (this.codecSelect && this.modal.codecSelect) this.modal.codecSelect.value = this.codecSelect.value;
    this.modal.audioCheckbox.checked = this.audioCheckbox.checked;

    this.modal.modalBackdrop.classList.remove("hidden");
    this.refreshSources();
  }

  public closeShareModal() {
    if (this.modal) {
      this.modal.modalBackdrop.classList.add("hidden");
    }
  }

  public async updateSourceSnapshot(sourceId: number) {
    try {
      if (this.sourcePreviewPlaceholder) {
        this.sourcePreviewPlaceholder.textContent = "Carregando miniatura...";
        this.sourcePreviewPlaceholder.classList.remove("hidden");
      }
      if (this.modal?.sourcePreviewPlaceholder) {
        this.modal.sourcePreviewPlaceholder.textContent = "Carregando miniatura...";
        this.modal.sourcePreviewPlaceholder.classList.remove("hidden");
      }
      const dataUrl = await invoke<string>("get_source_snapshot", { sourceId });
      if (this.sourcePreviewImg) {
        this.sourcePreviewImg.src = dataUrl;
        this.sourcePreviewImg.classList.remove("hidden");
      }
      if (this.sourcePreviewPlaceholder) {
        this.sourcePreviewPlaceholder.classList.add("hidden");
      }
      if (this.modal?.sourcePreviewImg) {
        this.modal.sourcePreviewImg.src = dataUrl;
        this.modal.sourcePreviewImg.classList.remove("hidden");
      }
      if (this.modal?.sourcePreviewPlaceholder) {
        this.modal.sourcePreviewPlaceholder.classList.add("hidden");
      }
    } catch (e) {
      console.warn("Falha ao obter miniatura da fonte:", e);
      if (this.sourcePreviewPlaceholder) {
        this.sourcePreviewPlaceholder.textContent = "Prévia indisponível para esta fonte";
        this.sourcePreviewPlaceholder.classList.remove("hidden");
      }
      if (this.modal?.sourcePreviewPlaceholder) {
        this.modal.sourcePreviewPlaceholder.textContent = "Prévia indisponível para esta fonte";
        this.modal.sourcePreviewPlaceholder.classList.remove("hidden");
      }
      if (this.sourcePreviewImg) {
        this.sourcePreviewImg.classList.add("hidden");
      }
      if (this.modal?.sourcePreviewImg) {
        this.modal.sourcePreviewImg.classList.add("hidden");
      }
    }
  }

  private updateAudioTag() {
    if (this.isAudioMuted) {
      this.statAudioTag.textContent = "Áudio mutado";
      this.btnToggleMute.textContent = "Desmutar";
    } else {
      this.statAudioTag.textContent = "Áudio ativo (Discord isolado)";
      this.btnToggleMute.textContent = "Mutar Áudio";
    }
  }

  public async refreshSources() {
    try {
      this.sourcesGrid.innerHTML = '<div class="loading-sources">Carregando fontes disponíveis...</div>';
      if (this.modal?.sourcesGrid) {
        this.modal.sourcesGrid.innerHTML = '<div class="loading-sources">Carregando fontes disponíveis...</div>';
      }
      const sources = await invoke<SourceInfo[]>("get_sources");
      this.currentSources = sources;

      // Seleciona a tela principal por padrão se nada estiver selecionado
      if (this.selectedSourceId === null) {
        const primary = sources.find((s) => s.kind === "Monitor" && s.primary);
        if (primary) {
          this.selectedSourceId = primary.id;
        } else if (sources.length > 0) {
          this.selectedSourceId = sources[0].id;
        }
      }

      if (this.selectedSourceId !== null) {
        this.updateSourceSnapshot(this.selectedSourceId);
      }

      this.renderSources();
    } catch (e) {
      this.sourcesGrid.innerHTML = `<div class="empty-peers">Erro ao listar fontes: ${e}</div>`;
      if (this.modal?.sourcesGrid) {
        this.modal.sourcesGrid.innerHTML = `<div class="empty-peers">Erro ao listar fontes: ${e}</div>`;
      }
    }
  }

  private renderSources() {
    const filtered = this.currentSources.filter((s) => {
      if (this.activeTab === "monitors") return s.kind === "Monitor";
      return s.kind === "Window";
    });

    this.renderSourceCardsInto(this.sourcesGrid, filtered);
    if (this.modal?.sourcesGrid) {
      this.renderSourceCardsInto(this.modal.sourcesGrid, filtered);
    }
  }

  private renderSourceCardsInto(grid: HTMLElement, sources: SourceInfo[]) {
    if (sources.length === 0) {
      grid.innerHTML = `
        <div class="empty-peers">
          Nenhuma fonte do tipo "${this.activeTab === "monitors" ? "Monitores" : "Janelas"}" encontrada.
        </div>
      `;
      return;
    }

    grid.innerHTML = "";
    for (const src of sources) {
      const card = document.createElement("div");
      const isSelected = src.id === this.selectedSourceId;
      card.className = `source-card ${isSelected ? "selected" : ""}`;

      if (src.kind === "Monitor") {
        card.innerHTML = `
          <div class="source-icon"><span class="material-symbols-rounded">desktop_windows</span></div>
          <div class="source-name" title="${this.escapeHtml(src.name)}">${this.escapeHtml(src.name)}</div>
          <div class="source-meta">${src.width}x${src.height} @ ${src.refresh_rate}Hz ${src.primary ? "• Principal" : ""}</div>
        `;
      } else {
        card.innerHTML = `
          <div class="source-icon"><span class="material-symbols-rounded">window</span></div>
          <div class="source-name" title="${this.escapeHtml(src.title)}">${this.escapeHtml(src.title)}</div>
          <div class="source-meta">${this.escapeHtml(src.exe)} • PID ${src.pid} (${src.refresh_rate}Hz)</div>
        `;
      }

      if (isSelected && this.fpsSelect && src.refresh_rate) {
        const autoOpt = this.fpsSelect.querySelector('option[value="auto"]');
        if (autoOpt) {
          autoOpt.textContent = `Taxa do Monitor (${src.refresh_rate} Hz)`;
        }
      }
      if (isSelected && this.modal?.fpsSelect && src.refresh_rate) {
        const autoOpt = this.modal.fpsSelect.querySelector('option[value="auto"]');
        if (autoOpt) {
          autoOpt.textContent = `Taxa do Monitor (${src.refresh_rate} Hz)`;
        }
      }

      card.addEventListener("click", () => {
        this.selectedSourceId = src.id;
        this.updateSourceSnapshot(src.id);
        if (this.fpsSelect && src.refresh_rate) {
          const autoOpt = this.fpsSelect.querySelector('option[value="auto"]');
          if (autoOpt) {
            autoOpt.textContent = `Taxa do Monitor (${src.refresh_rate} Hz)`;
          }
        }
        if (this.modal?.fpsSelect && src.refresh_rate) {
          const autoOpt = this.modal.fpsSelect.querySelector('option[value="auto"]');
          if (autoOpt) {
            autoOpt.textContent = `Taxa do Monitor (${src.refresh_rate} Hz)`;
          }
        }
        this.renderSources();
      });

      grid.appendChild(card);
    }
  }

  public async startShare(fromModal: boolean = false) {
    if (this.selectedSourceId === null) {
      alert("Selecione uma tela ou janela para transmitir.");
      return;
    }

    const resSelect = fromModal && this.modal ? this.modal.resolutionSelect : this.resolutionSelect;
    const audCheck = fromModal && this.modal ? this.modal.audioCheckbox : this.audioCheckbox;
    const fSelect = fromModal && this.modal ? this.modal.fpsSelect : this.fpsSelect;
    const bitSelect = fromModal && this.modal ? this.modal.bitrateSelect : this.bitrateSelect;
    const codSelect = fromModal && this.modal ? this.modal.codecSelect : this.codecSelect;

    const preset = resSelect.value;
    const audioEnabled = audCheck.checked;

    let fpsTarget: number | null = null;
    if (fSelect) {
      const val = fSelect.value;
      if (val === "auto") {
        fpsTarget = null;
      } else if (val === "unlocked") {
        fpsTarget = 0;
      } else {
        fpsTarget = parseInt(val, 10) || null;
      }
    }

    let bitrateKbps: number | null = null;
    if (bitSelect) {
      const bVal = bitSelect.value;
      if (bVal !== "auto") {
        bitrateKbps = parseInt(bVal, 10) || null;
      }
    }

    if (fromModal) {
      this.closeShareModal();
    }

    try {
      this.btnStart.disabled = true;
      this.btnStart.textContent = "Iniciando...";

      // Inicializa o StreamPlayer dedicado da pré-visualização (sem áudio local)
      if (this.previewPlayer) {
        this.previewPlayer.destroy();
      }
      this.hostPreviewOverlay.classList.remove("hidden");
      this.previewPlayer = new StreamPlayer(
        this.hostPreviewCanvas,
        this.hostPreviewOverlay,
        (stats) => {
          this.previewFpsTag.textContent = `${stats.fps} FPS`;
          this.previewResTag.textContent = `${stats.width}x${stats.height}`;
        },
        false // Áudio desabilitado no preview para evitar eco imediato
      );

      // Cria o canal Tauri v2 para o feed de vídeo espelhado em tempo real
      const onPreview = new Channel<StreamEvent>();
      onPreview.onmessage = (msg: StreamEvent) => {
        if (this.previewPlayer) {
          if (msg.t === "video") {
            this.previewPlayer.handleVideoFrame(msg.d, msg.k, msg.p);
          } else if (msg.t === "config") {
            if (msg.width && msg.height) {
              this.previewPlayer.setDimensions(msg.width, msg.height);
            }
            if (msg.codec) {
              this.previewPlayer.configureDecoder(msg.codec);
            }
          }
        }
        if (this.onPreviewStreamEvent) {
          this.onPreviewStreamEvent(msg);
        }
      };

      const codec = codSelect ? codSelect.value : "auto";

      const info = await invoke<ShareInfo>("start_share", {
        sourceId: this.selectedSourceId,
        presetStr: preset,
        fpsTarget,
        bitrateKbps,
        codec,
        audioMuted: !audioEnabled,
        onPreview,
      });

      if (this.previewPlayer) {
        if (info.width && info.height) {
          this.previewPlayer.setDimensions(info.width, info.height);
        }
        if (info.codec) {
          this.previewPlayer.configureDecoder(info.codec);
        }
      }

      this.currentShareInfo = info;
      this.isSharing = true;
      this.isAudioMuted = !audioEnabled;
      this.sourcesSelectionArea.classList.add("hidden");
      this.hostPreviewCard.classList.remove("hidden");
      this.btnStart.classList.add("hidden");
      this.btnStop.classList.remove("hidden");

      this.statPort.textContent = `${info.media_port}`;
      this.previewFpsTag.textContent = `${info.fps} FPS`;
      this.previewResTag.textContent = `${info.width}x${info.height}`;
      this.previewViewersCount.textContent = "0 assistindo";
      if (this.onViewersChange) {
        this.onViewersChange(0, [], "0 assistindo");
      }
      if (this.onViewersCountChange) {
        this.onViewersCountChange(0, "0 assistindo");
      }
      this.updateAudioTag();

      // Configura código da sala P2P (Iroh)
      if (info.room_ticket) {
        this.currentRoomTicket = info.room_ticket;
        if (this.shareTicketInput) {
          this.shareTicketInput.value = info.room_ticket;
        }
        if (this.roomTicketBox) {
          this.roomTicketBox.classList.remove("hidden");
        }
      } else {
        if (this.roomTicketBox) {
          this.roomTicketBox.classList.add("hidden");
        }
      }

      // Inicia polling de estatísticas de espectadores
      this.startStatsPolling();

      if (this.onStatusChange) {
        this.onStatusChange(true);
      }
    } catch (e) {
      if (this.previewPlayer) {
        this.previewPlayer.destroy();
        this.previewPlayer = null;
      }
      alert("Falha ao iniciar transmissão: " + e);
    } finally {
      this.btnStart.disabled = false;
      this.btnStart.innerHTML =
        '<span class="material-symbols-rounded">screen_share</span><span>Iniciar Transmissão</span>';
    }
  }

  public getIsSharing(): boolean {
    return this.isSharing;
  }

  public getRoomTicket(): string | null {
    return this.currentRoomTicket;
  }

  public getIsAudioMuted(): boolean {
    return this.isAudioMuted;
  }

  public async toggleAudioMute(): Promise<boolean> {
    this.isAudioMuted = !this.isAudioMuted;
    try {
      await invoke("set_audio_muted", { muted: this.isAudioMuted });
      this.updateAudioTag();
    } catch (e) {
      console.warn("Erro ao alternar mudo:", e);
    }
    return this.isAudioMuted;
  }

  public getSelectedSourceId(): number | null {
    return this.selectedSourceId;
  }

  private startStatsPolling() {
    this.stopStatsPolling();
    this.statsInterval = window.setInterval(async () => {
      if (!this.isSharing) return;
      try {
        const stats = await invoke<StreamStats>("get_stream_stats");
        if (stats) {
          const count = stats.viewers_count;
          const mb = (stats.bytes_sent / (1024 * 1024)).toFixed(1);
          const nacks = stats.nacks_served > 0 ? ` • ${stats.nacks_served} NACKs atendidos` : "";
          const text = `${count} ${count === 1 ? "assistindo" : "assistindo"} • ${mb} MB${nacks}`;
          this.previewViewersCount.textContent = text;
          if (this.onViewersChange) {
            this.onViewersChange(count, stats.viewers || [], text);
          }
          if (this.onViewersCountChange) {
            this.onViewersCountChange(count, text);
          }
        }
      } catch (e) {
        // Silencioso em caso de erro temporário
      }
    }, 1500);
  }

  private stopStatsPolling() {
    if (this.statsInterval !== null) {
      clearInterval(this.statsInterval);
      this.statsInterval = null;
    }
  }

  public async stopShare() {
    this.stopStatsPolling();
    try {
      await invoke("stop_share");
    } catch (e) {
      console.warn("Erro ao parar share:", e);
    } finally {
      this.isSharing = false;
      this.currentRoomTicket = null;
      this.currentShareInfo = null;
      if (this.onViewersChange) {
        this.onViewersChange(0, [], "0 assistindo");
      }
      if (this.roomTicketBox) {
        this.roomTicketBox.classList.add("hidden");
      }
      if (this.previewPlayer) {
        this.previewPlayer.destroy();
        this.previewPlayer = null;
      }
      this.hostPreviewCard.classList.add("hidden");
      this.sourcesSelectionArea.classList.remove("hidden");
      this.btnStart.classList.remove("hidden");
      this.btnStop.classList.add("hidden");

      if (this.onStatusChange) {
        this.onStatusChange(false);
      }
    }
  }

  public getShareInfo(): ShareInfo | null {
    return this.currentShareInfo;
  }

  private escapeHtml(str: string): string {
    return str.replace(/[&<>'"]/g, (c) => `&#${c.charCodeAt(0)};`);
  }
}
