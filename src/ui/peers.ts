import { invoke } from "@tauri-apps/api/core";

export interface PeerInfo {
  id: string;
  nick: string;
  status: "Idle" | "Streaming" | "Watching";
  ip: string;
  ctrl_port: number;
}

export class PeersManager {
  private peersContainer: HTMLElement;
  private peersCountBadge: HTMLElement;
  private manualIpInput: HTMLInputElement;
  private btnAddManual: HTMLButtonElement;
  private onWatchCallback: (ip: string, nick: string) => void;
  private knownPeers: PeerInfo[] = [];

  private btnPeersToggle = document.getElementById("btn-peers-toggle") as HTMLButtonElement | null;
  private peersPopover = document.getElementById("peers-popover") as HTMLElement | null;
  private btnClosePopover = document.getElementById("btn-close-peers-popover") as HTMLButtonElement | null;
  private livePeersStrip = document.getElementById("live-peers-strip") as HTMLElement | null;

  constructor(
    peersContainer: HTMLElement,
    peersCountBadge: HTMLElement,
    manualIpInput: HTMLInputElement,
    btnAddManual: HTMLButtonElement,
    onWatch: (ip: string, nick: string) => void
  ) {
    this.peersContainer = peersContainer;
    this.peersCountBadge = peersCountBadge;
    this.manualIpInput = manualIpInput;
    this.btnAddManual = btnAddManual;
    this.onWatchCallback = onWatch;

    this.bindEvents();
    this.startPolling();
  }

  private bindEvents() {
    const handleAdd = async () => {
      const val = this.manualIpInput.value.trim();
      if (!val) return;
      if (val.toLowerCase().startsWith("lk-")) {
        this.manualIpInput.value = "";
        this.closePopover();
        this.onWatchCallback(val, "Sala P2P");
        return;
      }
      try {
        await invoke("add_manual_peer", { ip: val });
        this.manualIpInput.value = "";
        await this.refreshPeers();
      } catch (_e) {
        this.manualIpInput.value = "";
        this.closePopover();
        this.onWatchCallback(val, val);
      }
    };

    this.btnAddManual.addEventListener("click", handleAdd);
    this.manualIpInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleAdd();
    });

    if (this.btnPeersToggle && this.peersPopover) {
      this.btnPeersToggle.addEventListener("click", (e) => {
        e.stopPropagation();
        const isHidden = this.peersPopover!.classList.toggle("hidden");
        this.btnPeersToggle!.classList.toggle("active", !isHidden);
      });
    }

    if (this.btnClosePopover) {
      this.btnClosePopover.addEventListener("click", () => this.closePopover());
    }

    document.addEventListener("click", (e) => {
      if (!this.peersPopover || this.peersPopover.classList.contains("hidden")) return;
      const target = e.target as Node;
      if (!this.peersPopover.contains(target) && this.btnPeersToggle && !this.btnPeersToggle.contains(target)) {
        this.closePopover();
      }
    });
  }

  private closePopover() {
    if (this.peersPopover) {
      this.peersPopover.classList.add("hidden");
    }
    if (this.btnPeersToggle) {
      this.btnPeersToggle.classList.remove("active");
    }
  }

  private startPolling() {
    this.refreshPeers();
    setInterval(() => this.refreshPeers(), 1500);
  }

  public async refreshPeers() {
    try {
      const peers = await invoke<PeerInfo[]>("list_peers");
      this.knownPeers = peers;
      this.render();
    } catch (e) {
      console.warn("Erro ao listar peers:", e);
    }
  }

  private render() {
    this.peersCountBadge.textContent = this.knownPeers.length.toString();

    // Renderiza chips rápidos na barra superior para amigos que estão transmitindo agora
    if (this.livePeersStrip) {
      const streamingPeers = this.knownPeers.filter((p) => p.status === "Streaming");
      this.livePeersStrip.innerHTML = "";
      for (const peer of streamingPeers) {
        const chip = document.createElement("button");
        chip.className = "live-peer-chip";
        chip.title = `Assistir transmissão de ${peer.nick} (${peer.ip})`;
        chip.innerHTML = `
          <span class="material-symbols-rounded ms-xs">sensors</span>
          <span>${this.escapeHtml(peer.nick)}</span>
          <span class="material-symbols-rounded ms-xs">play_arrow</span>
        `;
        chip.addEventListener("click", () => {
          this.onWatchCallback(peer.ip, peer.nick);
        });
        this.livePeersStrip.appendChild(chip);
      }
    }

    if (this.knownPeers.length === 0) {
      this.peersContainer.innerHTML = `
        <div class="empty-peers">
          Nenhum peer detectado na rede local.
          <div style="font-size: 0.76rem; margin-top: 8px; color: #38bdf8; display: flex; align-items: center; justify-content: center; gap: 5px;">
            <span class="material-symbols-rounded ms-xs">vpn_key</span>
            <span>Cole um <strong>Código de Sala (lk-...)</strong> na barra azul acima para assistir via Internet!</span>
          </div>
        </div>
      `;
      return;
    }

    this.peersContainer.innerHTML = "";
    for (const peer of this.knownPeers) {
      const card = document.createElement("div");
      card.className = "peer-card";

      const statusConfig: Record<string, { icon: string; label: string }> = {
        Idle: { icon: "person", label: "Ocioso" },
        Streaming: { icon: "sensors", label: "Transmitindo" },
        Watching: { icon: "visibility", label: "Assistindo" },
      };

      const st = statusConfig[peer.status] || { icon: "person", label: peer.status };
      const isStreaming = peer.status === "Streaming";

      card.innerHTML = `
        <div class="peer-card-header">
          <span class="peer-nick">
            <span class="material-symbols-rounded ms-sm">account_circle</span>
            ${this.escapeHtml(peer.nick)}
          </span>
          <span class="peer-status-tag ${peer.status}">
            <span class="material-symbols-rounded ms-xs">${st.icon}</span>
            <span>${st.label}</span>
          </span>
        </div>
        <div class="peer-card-body">
          <span class="peer-ip">${peer.ip}</span>
          ${
            isStreaming
              ? `<button class="btn-primary btn-sm btn-watch" data-ip="${peer.ip}" data-nick="${this.escapeHtml(peer.nick)}">
                   <span class="material-symbols-rounded ms-xs">play_arrow</span>
                   <span>Assistir</span>
                 </button>`
              : ""
          }
        </div>
      `;

      const btnWatch = card.querySelector(".btn-watch") as HTMLButtonElement | null;
      if (btnWatch) {
        btnWatch.addEventListener("click", () => {
          this.closePopover();
          this.onWatchCallback(peer.ip, peer.nick);
        });
      }

      this.peersContainer.appendChild(card);
    }
  }

  private escapeHtml(str: string): string {
    return str.replace(/[&<>'"]/g, (c) => `&#${c.charCodeAt(0)};`);
  }
}
