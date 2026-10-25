import { invoke } from "@tauri-apps/api/core";

export type FriendStatus = "PendingOut" | "PendingIn" | "Accepted";

export interface Friend {
  tag: string;
  nick: string;
  public_key_b58: string;
  status: FriendStatus;
  is_online: boolean;
  is_streaming: boolean;
}

export type SocialNotification =
  | { kind: "Toast"; message: string; icon: string }
  | {
      kind: "IncomingKnock";
      request_id: string;
      from_tag: string;
      from_nick: string;
    };

export interface KnockResult {
  accepted: boolean;
  room_ticket: string | null;
  host_nick: string;
  message: string;
}

export class FriendsUI {
  private myTag: string = "";
  private friends: Friend[] = [];
  private knockingTags = new Set<string>();
  private activeKnockCards = new Map<string, HTMLElement>();
  private shownFriendReqCards = new Map<string, HTMLElement>();

  private btnToggle: HTMLButtonElement | null;
  private popover: HTMLElement | null;
  private myTagHeaderLabel: HTMLElement | null;
  private myTagPopoverVal: HTMLElement | null;
  private btnCopyMyTag: HTMLButtonElement | null;
  private badgeEl: HTMLElement | null;
  private addInput: HTMLInputElement | null;
  private btnSendReq: HTMLButtonElement | null;
  private pendingSection: HTMLElement | null;
  private pendingList: HTMLElement | null;
  private friendsList: HTMLElement | null;
  private knockContainer: HTMLElement | null;

  private onJoinApprovedStream: (roomTicket: string, hostNick: string) => void;
  private showToast: (msg: string, icon?: string) => void;

  constructor(
    myTag: string,
    onJoinApprovedStream: (roomTicket: string, hostNick: string) => void,
    showToast: (msg: string, icon?: string) => void
  ) {
    this.myTag = myTag;
    this.onJoinApprovedStream = onJoinApprovedStream;
    this.showToast = showToast;

    this.btnToggle = document.getElementById("btn-friends-toggle") as HTMLButtonElement | null;
    this.popover = document.getElementById("friends-popover");
    this.myTagHeaderLabel = document.getElementById("my-tag-label");
    this.myTagPopoverVal = document.getElementById("popover-my-tag-val");
    this.btnCopyMyTag = document.getElementById("btn-copy-my-tag") as HTMLButtonElement | null;
    this.badgeEl = document.getElementById("friends-badge");
    this.addInput = document.getElementById("add-friend-tag-input") as HTMLInputElement | null;
    this.btnSendReq = document.getElementById("btn-send-friend-req") as HTMLButtonElement | null;
    this.pendingSection = document.getElementById("friends-pending-section");
    this.pendingList = document.getElementById("friends-pending-list");
    this.friendsList = document.getElementById("friends-accepted-list");
    this.knockContainer = document.getElementById("knock-toast-container");

    this.setMyTag(myTag);
    this.bindEvents();
    this.refreshFriends();

    // Atualiza lista e presença P2P
    window.setInterval(() => this.refreshFriends(), 3500);
    // Drena notificações sociais e toasts de Knock em tempo real
    window.setInterval(() => this.pollNotifications(), 1100);
  }

  public setMyTag(tag: string) {
    if (!tag) return;
    this.myTag = tag;
    if (this.myTagHeaderLabel) {
      this.myTagHeaderLabel.textContent = tag;
    }
    if (this.myTagPopoverVal) {
      this.myTagPopoverVal.textContent = tag;
    }
  }

  private bindEvents() {
    if (this.btnToggle && this.popover) {
      this.btnToggle.addEventListener("click", (e) => {
        e.stopPropagation();
        const isHidden = this.popover!.classList.toggle("hidden");
        this.btnToggle!.classList.toggle("active", !isHidden);
        if (!isHidden) {
          this.refreshFriends();
        }
      });

      document.addEventListener("click", (e) => {
        if (!this.popover || this.popover.classList.contains("hidden")) return;
        const target = e.target as Node;
        if (!this.popover.contains(target) && !this.btnToggle?.contains(target)) {
          this.popover.classList.add("hidden");
          this.btnToggle?.classList.remove("active");
        }
      });
    }

    if (this.btnCopyMyTag) {
      this.btnCopyMyTag.addEventListener("click", () => {
        if (!this.myTag) return;
        navigator.clipboard.writeText(this.myTag);
        this.showToast(`Sua tag ${this.myTag} foi copiada!`, "content_copy");
      });
    }

    const handleSendReq = async () => {
      if (!this.addInput || !this.btnSendReq) return;
      const raw = this.addInput.value.trim().toUpperCase();
      if (!raw) return;

      this.btnSendReq.disabled = true;
      try {
        const msg = await invoke<string>("send_friend_request", { targetTag: raw });
        this.addInput.value = "";
        this.showToast(msg, "person_add");
        await this.refreshFriends();
      } catch (e: any) {
        this.showToast(String(e), "error");
      } finally {
        this.btnSendReq.disabled = false;
      }
    };

    if (this.btnSendReq) {
      this.btnSendReq.addEventListener("click", handleSendReq);
    }
    if (this.addInput) {
      this.addInput.addEventListener("keydown", (e) => {
        if (e.key === "Enter") handleSendReq();
      });
    }
  }

  public async refreshFriends() {
    try {
      const list = await invoke<Friend[]>("list_friends");
      this.friends = list || [];
      this.render();
    } catch (e) {
      console.debug("Erro ao listar amigos:", e);
    }
  }

  private async pollNotifications() {
    try {
      const items = await invoke<SocialNotification[]>("poll_social_notifications");
      if (!items || items.length === 0) return;

      let shouldRefresh = false;
      for (const item of items) {
        if (item.kind === "Toast") {
          this.showToast(item.message, item.icon || "person");
          shouldRefresh = true;
        } else if (item.kind === "IncomingKnock") {
          this.renderIncomingKnockToast(item.request_id, item.from_tag, item.from_nick);
        }
      }
      if (shouldRefresh) {
        this.refreshFriends();
      }
    } catch (_) {}
  }

  private renderIncomingKnockToast(requestId: string, fromTag: string, fromNick: string) {
    if (!this.knockContainer || this.activeKnockCards.has(requestId)) return;

    const card = document.createElement("div");
    card.className = "knock-toast-card";
    card.innerHTML = `
      <div class="knock-toast-top">
        <div class="knock-avatar">
          <span class="material-symbols-rounded ms-sm">doorbell</span>
        </div>
        <div class="knock-info">
          <div class="knock-title">
            <strong>${this.escapeHtml(fromNick)}</strong>
            <span class="knock-tag">${this.escapeHtml(fromTag)}</span>
          </div>
          <div class="knock-subtitle">Pediu permissão para entrar na sua transmissão P2P</div>
        </div>
      </div>
      <div class="knock-actions">
        <button class="btn-knock-reject" type="button">
          <span class="material-symbols-rounded ms-xs">close</span>
          <span>Recusar</span>
        </button>
        <button class="btn-knock-accept" type="button">
          <span class="material-symbols-rounded ms-xs">check</span>
          <span>Permitir Entrada</span>
        </button>
      </div>
    `;

    const dismiss = () => {
      this.activeKnockCards.delete(requestId);
      card.style.opacity = "0";
      card.style.transform = "translateY(-8px)";
      setTimeout(() => card.remove(), 200);
    };

    const btnAccept = card.querySelector(".btn-knock-accept") as HTMLButtonElement;
    const btnReject = card.querySelector(".btn-knock-reject") as HTMLButtonElement;

    btnAccept.addEventListener("click", async () => {
      btnAccept.disabled = true;
      btnReject.disabled = true;
      try {
        await invoke("respond_knock_request", { requestId, accept: true });
        this.showToast(`Entrada liberada para ${fromNick}!`, "check_circle");
      } catch (_) {}
      dismiss();
    });

    btnReject.addEventListener("click", async () => {
      btnAccept.disabled = true;
      btnReject.disabled = true;
      try {
        await invoke("respond_knock_request", { requestId, accept: false });
      } catch (_) {}
      dismiss();
    });

    this.activeKnockCards.set(requestId, card);
    this.knockContainer.appendChild(card);

    // Expira visualmente após 30s (mesmo timeout do canal oneshot no Rust)
    window.setTimeout(() => {
      if (this.activeKnockCards.has(requestId)) {
        dismiss();
      }
    }, 30000);
  }

  private renderIncomingFriendReqCard(req: Friend) {
    if (!this.knockContainer || this.shownFriendReqCards.has(req.tag)) return;

    const card = document.createElement("div");
    card.className = "knock-toast-card";
    card.innerHTML = `
      <div class="knock-toast-top">
        <div class="knock-avatar">
          <span class="material-symbols-rounded ms-sm">person_add</span>
        </div>
        <div class="knock-info">
          <div class="knock-title">
            <strong>${this.escapeHtml(req.nick)}</strong>
            <span class="knock-tag">${this.escapeHtml(req.tag)}</span>
          </div>
          <div class="knock-subtitle">Enviou um pedido de amizade para você</div>
        </div>
      </div>
      <div class="knock-actions">
        <button class="btn-knock-reject" type="button">
          <span class="material-symbols-rounded ms-xs">close</span>
          <span>Recusar</span>
        </button>
        <button class="btn-knock-accept" type="button">
          <span class="material-symbols-rounded ms-xs">check</span>
          <span>Aceitar Amizade</span>
        </button>
      </div>
    `;

    const dismiss = () => {
      card.style.opacity = "0";
      card.style.transform = "translateY(-8px)";
      setTimeout(() => card.remove(), 200);
    };

    const btnAccept = card.querySelector(".btn-knock-accept") as HTMLButtonElement;
    const btnReject = card.querySelector(".btn-knock-reject") as HTMLButtonElement;

    btnAccept.addEventListener("click", async () => {
      btnAccept.disabled = true;
      btnReject.disabled = true;
      try {
        await invoke("respond_friend_request", { friendTag: req.tag, accept: true });
        this.showToast(`Amizade confirmada com ${req.nick} (${req.tag})!`, "how_to_reg");
        await this.refreshFriends();
      } catch (e: any) {
        this.showToast(String(e), "error");
      }
      dismiss();
    });

    btnReject.addEventListener("click", async () => {
      btnAccept.disabled = true;
      btnReject.disabled = true;
      try {
        await invoke("respond_friend_request", { friendTag: req.tag, accept: false });
        await this.refreshFriends();
      } catch (_) {}
      dismiss();
    });

    this.shownFriendReqCards.set(req.tag, card);
    this.knockContainer.appendChild(card);
  }

  private render() {
    const pendingIn = this.friends.filter((f) => f.status === "PendingIn");
    const pendingTags = new Set(pendingIn.map((f) => f.tag));
    for (const [tag, card] of this.shownFriendReqCards.entries()) {
      if (!pendingTags.has(tag)) {
        card.remove();
        this.shownFriendReqCards.delete(tag);
      }
    }
    for (const req of pendingIn) {
      this.renderIncomingFriendReqCard(req);
    }
    const others = this.friends
      .filter((f) => f.status !== "PendingIn")
      .sort((a, b) => {
        const score = (x: Friend) => {
          if (x.status === "Accepted" && x.is_streaming) return 0;
          if (x.status === "Accepted" && x.is_online) return 1;
          if (x.status === "Accepted") return 2;
          return 3;
        };
        return score(a) - score(b) || a.nick.localeCompare(b.nick);
      });

    const liveCount = others.filter((f) => f.status === "Accepted" && f.is_streaming).length;
    const badgeTotal = pendingIn.length + liveCount;

    if (this.badgeEl) {
      if (badgeTotal > 0) {
        this.badgeEl.textContent = `${badgeTotal}`;
        this.badgeEl.classList.remove("hidden");
        this.badgeEl.classList.toggle("badge-live-alert", pendingIn.length > 0 || liveCount > 0);
      } else {
        this.badgeEl.classList.add("hidden");
      }
    }

    // Seção de convites pendentes recebidos
    if (this.pendingSection && this.pendingList) {
      if (pendingIn.length === 0) {
        this.pendingSection.classList.add("hidden");
        this.pendingList.innerHTML = "";
      } else {
        this.pendingSection.classList.remove("hidden");
        this.pendingList.innerHTML = "";
        for (const req of pendingIn) {
          const row = document.createElement("div");
          row.className = "friend-row pending-in-row";
          row.innerHTML = `
            <div class="friend-meta">
              <div class="friend-name-line">
                <span class="friend-nick">${this.escapeHtml(req.nick)}</span>
                <span class="friend-tag-chip">${this.escapeHtml(req.tag)}</span>
              </div>
              <span class="friend-substatus">Quer adicionar você como amigo</span>
            </div>
            <div class="friend-row-actions">
              <button class="btn-friend-accept" title="Confirmar amizade mútua">
                <span class="material-symbols-rounded ms-xs">check</span>
                <span>Aceitar</span>
              </button>
              <button class="btn-friend-icon danger" title="Recusar pedido">
                <span class="material-symbols-rounded ms-xs">close</span>
              </button>
            </div>
          `;

          const btnAcc = row.querySelector(".btn-friend-accept") as HTMLButtonElement;
          const btnRej = row.querySelector(".btn-friend-icon.danger") as HTMLButtonElement;

          btnAcc.addEventListener("click", async () => {
            btnAcc.disabled = true;
            try {
              await invoke("respond_friend_request", { friendTag: req.tag, accept: true });
              this.showToast(`Amizade confirmada com ${req.nick} (${req.tag})!`, "how_to_reg");
              await this.refreshFriends();
            } catch (e: any) {
              this.showToast(String(e), "error");
            }
          });

          btnRej.addEventListener("click", async () => {
            btnRej.disabled = true;
            try {
              await invoke("respond_friend_request", { friendTag: req.tag, accept: false });
              await this.refreshFriends();
            } catch (_) {}
          });

          this.pendingList.appendChild(row);
        }
      }
    }

    // Lista principal de amigos confirmados + enviados aguardando aceite
    if (this.friendsList) {
      if (others.length === 0) {
        this.friendsList.innerHTML = `
          <div class="friends-empty">
            <span class="material-symbols-rounded">group_add</span>
            <span>Adicione amigos pela tag <strong>LK#XXXX</strong> acima. A amizade só ativa após ambos aceitarem.</span>
          </div>
        `;
      } else {
        this.friendsList.innerHTML = "";
        for (const f of others) {
          const row = document.createElement("div");
          row.className = `friend-row ${f.is_streaming ? "is-live" : ""}`;

          let statusHtml = "";
          if (f.status === "PendingOut") {
            statusHtml = `<span class="friend-substatus pending">Convite enviado • Aguardando aceite</span>`;
          } else if (f.is_streaming) {
            statusHtml = `<span class="friend-live-pill"><span class="live-dot"></span>AO VIVO</span>`;
          } else if (f.is_online) {
            statusHtml = `<span class="friend-substatus online"><span class="online-dot"></span>Online no P2P</span>`;
          } else {
            statusHtml = `<span class="friend-substatus">Offline</span>`;
          }

          const isKnocking = this.knockingTags.has(f.tag);
          const knockBtnHtml =
            f.status === "Accepted" && f.is_streaming
              ? `<button class="btn-friend-knock" ${isKnocking ? "disabled" : ""} title="Pedir permissão ao Host para entrar na live">
                   <span class="material-symbols-rounded ms-xs">${isKnocking ? "hourglass_top" : "meeting_room"}</span>
                   <span>${isKnocking ? "Aguardando..." : "Pedir p/ Entrar"}</span>
                 </button>`
              : "";

          row.innerHTML = `
            <div class="friend-meta">
              <div class="friend-name-line">
                <span class="friend-nick">${this.escapeHtml(f.nick)}</span>
                <span class="friend-tag-chip">${this.escapeHtml(f.tag)}</span>
              </div>
              ${statusHtml}
            </div>
            <div class="friend-row-actions">
              ${knockBtnHtml}
              <button class="btn-friend-icon remove" title="Remover amigo">
                <span class="material-symbols-rounded ms-xs">person_remove</span>
              </button>
            </div>
          `;

          const btnKnock = row.querySelector(".btn-friend-knock") as HTMLButtonElement | null;
          if (btnKnock) {
            btnKnock.addEventListener("click", async () => {
              this.knockingTags.add(f.tag);
              this.render();
              try {
                const res = await invoke<KnockResult>("knock_friend_stream", { friendTag: f.tag });
                if (res.accepted && res.room_ticket) {
                  this.popover?.classList.add("hidden");
                  this.btnToggle?.classList.remove("active");
                  this.showToast(res.message, "check_circle");
                  this.onJoinApprovedStream(res.room_ticket, res.host_nick || f.nick);
                } else {
                  this.showToast(res.message || "Pedido recusado pelo Host.", "info");
                }
              } catch (e: any) {
                this.showToast(String(e), "error");
              } finally {
                this.knockingTags.delete(f.tag);
                this.render();
              }
            });
          }

          const btnRemove = row.querySelector(".btn-friend-icon.remove") as HTMLButtonElement;
          btnRemove.addEventListener("click", async () => {
            try {
              await invoke("remove_friend", { friendTag: f.tag });
              await this.refreshFriends();
            } catch (_) {}
          });

          this.friendsList.appendChild(row);
        }
      }
    }
  }

  private escapeHtml(str: string): string {
    return str.replace(/[&<>'"]/g, (c) => `&#${c.charCodeAt(0)};`);
  }
}
