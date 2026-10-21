import { invoke } from "@tauri-apps/api/core";

export interface PeerInfo {
  id: string;
  nick: string;
  status: "Idle" | "Streaming" | "Watching";
  ip: string;
  ctrl_port: number;
}

export interface FriendInfo {
  tag: string;
  nick: string;
  public_key_b58: string;
  status: "PendingOut" | "PendingIn" | "Accepted" | "Blocked";
  is_online: boolean;
  is_streaming: boolean;
  room_code: string | null;
}

export class PeersManager {
  private peersContainer: HTMLElement;
  private peersCountBadge: HTMLElement;
  private manualIpInput: HTMLInputElement;
  private btnAddManual: HTMLButtonElement;
  private onWatchCallback: (ip: string, nick: string) => void;
  private onToast?: (msg: string, icon?: string) => void;
  private knownPeers: PeerInfo[] = [];
  private knownFriends: FriendInfo[] = [];

  private btnPeersToggle = document.getElementById("btn-peers-toggle") as HTMLButtonElement | null;
  private peersPopover = document.getElementById("peers-popover") as HTMLElement | null;
  private btnClosePopover = document.getElementById("btn-close-peers-popover") as HTMLButtonElement | null;
  private livePeersStrip = document.getElementById("live-peers-strip") as HTMLElement | null;

  private friendsContainer = document.getElementById("friends-list") as HTMLElement | null;
  private addFriendInput = document.getElementById("add-friend-input") as HTMLInputElement | null;
  private btnSubmitAddFriend = document.getElementById("btn-submit-add-friend") as HTMLButtonElement | null;

  constructor(
    peersContainer: HTMLElement,
    peersCountBadge: HTMLElement,
    manualIpInput: HTMLInputElement,
    btnAddManual: HTMLButtonElement,
    onWatch: (ip: string, nick: string) => void,
    onToast?: (msg: string, icon?: string) => void
  ) {
    this.peersContainer = peersContainer;
    this.peersCountBadge = peersCountBadge;
    this.manualIpInput = manualIpInput;
    this.btnAddManual = btnAddManual;
    this.onWatchCallback = onWatch;
    this.onToast = onToast;

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

    if (this.btnSubmitAddFriend && this.addFriendInput) {
      const handleAddFriend = async () => {
        const tag = this.addFriendInput!.value.trim().toUpperCase();
        if (!tag) return;
        try {
          const res = await invoke<string>("send_friend_request", { targetTag: tag });
          this.addFriendInput!.value = "";
          if (this.onToast) this.onToast(res, "person_add");
          await this.refreshFriends();
        } catch (e) {
          alert("Erro ao adicionar amigo: " + e);
        }
      };

      this.btnSubmitAddFriend.addEventListener("click", handleAddFriend);
      this.addFriendInput.addEventListener("keydown", (e) => {
        if (e.key === "Enter") handleAddFriend();
      });
    }

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
    this.refreshFriends();
    setInterval(() => {
      this.refreshPeers();
      this.refreshFriends();
    }, 2000);
  }

  public async refreshFriends() {
    try {
      const notifications = await invoke<string[]>("sync_friends_inbox");
      if (notifications && notifications.length > 0 && this.onToast) {
        for (const notif of notifications) {
          this.onToast(notif, "notifications");
        }
      }

      const friends = await invoke<FriendInfo[]>("list_friends");
      this.knownFriends = friends;
      this.renderFriends();
    } catch (e) {
      console.warn("Erro ao sincronizar amigos:", e);
    }
  }

  private renderFriends() {
    if (!this.friendsContainer) return;
    if (this.knownFriends.length === 0) {
      this.friendsContainer.innerHTML =
        '<div class="empty-peers">Nenhum amigo adicionado ainda. Digite a Tag do amigo acima para adicionar!</div>';
      return;
    }

    this.friendsContainer.innerHTML = "";
    for (const f of this.knownFriends) {
      const item = document.createElement("div");
      item.className = "friend-item-card";

      let statusBadge = "";
      let actionsHtml = "";

      if (f.status === "PendingIn") {
        statusBadge = `<span style="color:#f59e0b; font-size:0.7rem; font-weight:700;">Solicitação Recebida</span>`;
        actionsHtml = `
          <button class="btn-primary btn-xs btn-accept" data-tag="${f.tag}" title="Aceitar Amizade">Aceitar</button>
          <button class="btn-secondary btn-xs btn-reject" data-tag="${f.tag}" title="Recusar">Recusar</button>
        `;
      } else if (f.status === "PendingOut") {
        statusBadge = `<span style="color:#94a3b8; font-size:0.7rem;">Pendente...</span>`;
        actionsHtml = `
          <button class="btn-secondary btn-xs btn-remove" data-tag="${f.tag}" title="Cancelar Pedido">Cancelar</button>
        `;
      } else if (f.status === "Accepted") {
        if (f.is_streaming && f.room_code) {
          statusBadge = `<span style="color:#f43f5e; font-size:0.7rem; font-weight:800; display:inline-flex; align-items:center; gap:3px;">
            <span class="material-symbols-rounded" style="font-size:12px;">sensors</span> EM LIVE
          </span>`;
          actionsHtml = `
            <button class="btn-knock-live btn-knock" data-tag="${f.tag}" data-room="${f.room_code}" data-nick="${this.escapeHtml(f.nick)}">
              <span class="material-symbols-rounded" style="font-size:14px;">play_arrow</span>
              <span>Assistir</span>
            </button>
          `;
        } else if (f.is_online) {
          statusBadge = `<span style="color:#10b981; font-size:0.7rem;">Online</span>`;
        } else {
          statusBadge = `<span style="color:#64748b; font-size:0.7rem;">Offline</span>`;
        }
      }

      item.innerHTML = `
        <div class="friend-info-col">
          <div class="friend-nick-row">
            <span class="material-symbols-rounded ms-xs" style="color:var(--accent-cyan-bright);">account_circle</span>
            <span>${this.escapeHtml(f.nick || f.tag)}</span>
            ${statusBadge}
          </div>
          <div class="friend-tag-sub">${f.tag}</div>
        </div>
        <div class="friend-actions-row">
          ${actionsHtml}
        </div>
      `;

      const btnAccept = item.querySelector(".btn-accept") as HTMLButtonElement | null;
      if (btnAccept) {
        btnAccept.addEventListener("click", async () => {
          await invoke("respond_friend_request", { friendTag: f.tag, accept: true });
          if (this.onToast) this.onToast(`Você agora é amigo de ${f.nick}!`, "check_circle");
          this.refreshFriends();
        });
      }

      const btnReject = item.querySelector(".btn-reject") as HTMLButtonElement | null;
      if (btnReject) {
        btnReject.addEventListener("click", async () => {
          await invoke("respond_friend_request", { friendTag: f.tag, accept: false });
          this.refreshFriends();
        });
      }

      const btnRemove = item.querySelector(".btn-remove") as HTMLButtonElement | null;
      if (btnRemove) {
        btnRemove.addEventListener("click", async () => {
          await invoke("remove_friend", { friendTag: f.tag });
          this.refreshFriends();
        });
      }

      const btnKnock = item.querySelector(".btn-knock") as HTMLButtonElement | null;
      if (btnKnock) {
        btnKnock.addEventListener("click", () => {
          this.closePopover();
          if (f.room_code) {
            this.onWatchCallback(f.room_code, f.nick);
          }
        });
      }

      this.friendsContainer.appendChild(item);
    }
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
