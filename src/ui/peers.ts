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
      if (val.startsWith("lk-")) {
        // Conexão direta à sala P2P (Iroh)
        this.manualIpInput.value = "";
        this.onWatchCallback(val, "Sala P2P");
        return;
      }
      try {
        await invoke("add_manual_peer", { ip: val });
        this.manualIpInput.value = "";
        await this.refreshPeers();
      } catch (e) {
        // Se falhar ao adicionar na tabela manual, tenta assistir diretamente
        this.manualIpInput.value = "";
        this.onWatchCallback(val, val);
      }
    };

    this.btnAddManual.addEventListener("click", handleAdd);
    this.manualIpInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleAdd();
    });
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

    if (this.knownPeers.length === 0) {
      this.peersContainer.innerHTML = `
        <div class="empty-peers">
          Nenhum peer detectado na rede local.
          <div style="font-size: 0.78rem; margin-top: 8px; color: #60a5fa;">
            ✨ Cole um <strong>Código de Sala (lk-...)</strong> no campo acima para assistir transmissões diretamente pela Internet!
          </div>
        </div>
      `;
      return;
    }

    this.peersContainer.innerHTML = "";
    for (const peer of this.knownPeers) {
      const card = document.createElement("div");
      card.className = "peer-card";

      const statusLabels = {
        Idle: "Ocioso",
        Streaming: "🔴 Transmitindo",
        Watching: "🟡 Assistindo",
      };

      const isStreaming = peer.status === "Streaming";

      card.innerHTML = `
        <div class="peer-card-header">
          <span class="peer-nick">${this.escapeHtml(peer.nick)}</span>
          <span class="peer-status-tag ${peer.status}">
            ${statusLabels[peer.status] || peer.status}
          </span>
        </div>
        <div class="peer-card-body">
          <span class="peer-ip">${peer.ip}</span>
          ${
            isStreaming
              ? `<button class="btn-primary btn-sm btn-watch" data-ip="${peer.ip}" data-nick="${this.escapeHtml(peer.nick)}">Assistir</button>`
              : ""
          }
        </div>
      `;

      const btnWatch = card.querySelector<HTMLButtonElement>(".btn-watch");
      if (btnWatch) {
        btnWatch.addEventListener("click", () => {
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
