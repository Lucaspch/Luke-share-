import { invoke } from "@tauri-apps/api/core";
import { Friend } from "./friends";

export interface ParticipantView {
  tag: string;
  nick: string;
  is_self: boolean;
  screen_ticket: string | null;
  camera_ticket: string | null;
  has_screen: boolean;
  has_camera: boolean;
}

export interface GroupRoomView {
  id: string;
  name: string;
  participants: ParticipantView[];
}

export interface GroupView {
  id: string;
  name: string;
  owner_tag: string;
  is_owner: boolean;
  rooms: GroupRoomView[];
  members_count: number;
  online_members_count: number;
}

export interface PendingGroupInviteSummary {
  group_id: string;
  group_name: string;
  from_tag: string;
  from_nick: string;
  rooms_count: number;
  members_count: number;
}

export interface GroupsStateSnapshot {
  groups: GroupView[];
  active_group_id: string | null;
  active_room_id: string | null;
  pending_invites: PendingGroupInviteSummary[];
}

export interface GroupInviteBundle {
  short_code: string | null;
  direct_ticket: string;
}

export interface DesiredGroupStream {
  streamKey: string;
  ticket: string;
  nick: string;
  kind: "screen" | "camera";
}

export class GroupsUI {
  private railList = document.getElementById("groups-rail-list") as HTMLElement;
  private btnNavPrivate = document.getElementById("btn-nav-private") as HTMLButtonElement;
  private btnAddGroup = document.getElementById("btn-add-group") as HTMLButtonElement;
  private railInviteBadge = document.getElementById("rail-group-invites-badge") as HTMLElement | null;

  // Sidebar do Grupo
  private groupSidebar = document.getElementById("group-sidebar") as HTMLElement;
  private groupTitleEl = document.getElementById("group-sidebar-name") as HTMLElement;
  private groupMetaEl = document.getElementById("group-sidebar-meta") as HTMLElement;
  private roomsListEl = document.getElementById("group-rooms-list") as HTMLElement;
  private btnCreateRoom = document.getElementById("btn-create-group-room") as HTMLButtonElement;
  private btnInviteGroup = document.getElementById("btn-group-invite") as HTMLButtonElement;
  private btnLeaveGroup = document.getElementById("btn-group-leave") as HTMLButtonElement;
  private btnCollapseSidebar = document.getElementById("btn-collapse-group-sidebar") as HTMLButtonElement;

  // Cartão de status conectado na sala no rodapé da sidebar
  private roomFooterEl = document.getElementById("group-room-footer") as HTMLElement;
  private roomFooterTitle = document.getElementById("group-connected-room-name") as HTMLElement;
  private btnExitRoom = document.getElementById("btn-exit-group-room") as HTMLButtonElement;
  private btnQuickScreen = document.getElementById("btn-group-quick-screen") as HTMLButtonElement;
  private btnQuickCam = document.getElementById("btn-group-quick-cam") as HTMLButtonElement;

  // Banner de convites pendentes dentro do modal/sidebar
  private pendingInvitesArea = document.getElementById("group-pending-invites-area") as HTMLElement;

  // Modal Criar / Entrar em Grupo
  private groupModal = document.getElementById("group-modal") as HTMLElement;
  private btnCloseGroupModal = document.getElementById("btn-close-group-modal") as HTMLButtonElement;
  private createGroupNameInput = document.getElementById("create-group-name-input") as HTMLInputElement;
  private btnConfirmCreateGroup = document.getElementById("btn-confirm-create-group") as HTMLButtonElement;
  private joinGroupCodeInput = document.getElementById("join-group-code-input") as HTMLInputElement;
  private btnConfirmJoinGroup = document.getElementById("btn-confirm-join-group") as HTMLButtonElement;

  // Modal Nova Sala
  private roomModal = document.getElementById("group-room-modal") as HTMLElement;
  private btnCloseRoomModal = document.getElementById("btn-close-room-modal") as HTMLButtonElement;
  private newRoomNameInput = document.getElementById("new-group-room-input") as HTMLInputElement;
  private btnConfirmCreateRoom = document.getElementById("btn-confirm-create-room") as HTMLButtonElement;

  // Modal Convidar para Grupo
  private inviteModal = document.getElementById("group-invite-modal") as HTMLElement;
  private btnCloseInviteModal = document.getElementById("btn-close-invite-modal") as HTMLButtonElement;
  private inviteCodeDisplay = document.getElementById("group-invite-short-code") as HTMLInputElement;
  private btnCopyInviteCode = document.getElementById("btn-copy-group-short-code") as HTMLButtonElement;
  private inviteTicketDisplay = document.getElementById("group-invite-direct-ticket") as HTMLInputElement;
  private btnCopyInviteTicket = document.getElementById("btn-copy-group-direct-ticket") as HTMLButtonElement;
  private inviteFriendsList = document.getElementById("group-invite-friends-list") as HTMLElement;

  private state: GroupsStateSnapshot = {
    groups: [],
    active_group_id: null,
    active_room_id: null,
    pending_invites: [],
  };
  private autoWatchCheckbox = document.getElementById("group-autowatch-checkbox") as HTMLInputElement | null;
  private isSidebarCollapsed = false;
  private autoWatch = localStorage.getItem("luke_group_auto_watch") !== "false";
  private dismissedTickets = new Set<string>();
  private manualTickets = new Set<string>();
  private lastRoomKey: string | null = null;
  private lastOpener: HTMLElement | null = null;
  private pollTimer: number | null = null;

  private onToast: (msg: string, icon?: string) => void;
  private onDesiredRoomStreams: (
    inRoom: boolean,
    groupName: string | null,
    roomName: string | null,
    desired: DesiredGroupStream[],
    allAvailable: DesiredGroupStream[]
  ) => void;
  private onModeChange: (mode: "private" | "group") => void;
  private onRequestScreenShare: () => void;
  private onRequestWebcamToggle: () => void;

  constructor(opts: {
    onToast: (msg: string, icon?: string) => void;
    onDesiredRoomStreams: (
      inRoom: boolean,
      groupName: string | null,
      roomName: string | null,
      desired: DesiredGroupStream[],
      allAvailable: DesiredGroupStream[]
    ) => void;
    onModeChange: (mode: "private" | "group") => void;
    onRequestScreenShare: () => void;
    onRequestWebcamToggle: () => void;
  }) {
    this.onToast = opts.onToast;
    this.onDesiredRoomStreams = opts.onDesiredRoomStreams;
    this.onModeChange = opts.onModeChange;
    this.onRequestScreenShare = opts.onRequestScreenShare;
    this.onRequestWebcamToggle = opts.onRequestWebcamToggle;

    this.bindEvents();
    this.refreshState();
    this.pollTimer = window.setInterval(() => this.refreshState(), 1800);
  }

  public getState(): GroupsStateSnapshot {
    return this.state;
  }

  public isInGroupRoom(): boolean {
    return Boolean(this.state.active_group_id && this.state.active_room_id);
  }

  public updateQuickTrackButtons(screenActive: boolean, camActive: boolean) {
    if (this.btnQuickScreen) {
      this.btnQuickScreen.classList.toggle("active", screenActive);
      this.btnQuickScreen.setAttribute("aria-pressed", String(screenActive));
    }
    if (this.btnQuickCam) {
      this.btnQuickCam.classList.toggle("active", camActive);
      this.btnQuickCam.setAttribute("aria-pressed", String(camActive));
    }
  }

  private bindEvents() {
    this.btnNavPrivate.addEventListener("click", async () => {
      await this.switchToPrivate();
    });

    this.btnAddGroup.addEventListener("click", () => {
      this.openModal(this.groupModal, this.btnAddGroup);
      this.renderPendingInvites();
      this.createGroupNameInput.focus();
    });

    this.btnCloseGroupModal.addEventListener("click", () => this.closeModal(this.groupModal));
    this.groupModal.addEventListener("click", (e) => {
      if (e.target === this.groupModal) this.closeModal(this.groupModal);
    });

    const handleCreateGroup = async () => {
      const name = this.createGroupNameInput.value.trim();
      if (!name) {
        this.onToast("Digite um nome para o grupo.", "edit");
        this.createGroupNameInput.focus();
        return;
      }
      this.btnConfirmCreateGroup.disabled = true;
      try {
        const g = await invoke<GroupView>("create_group", { name });
        this.createGroupNameInput.value = "";
        this.closeModal(this.groupModal);
        this.onModeChange("group");
        this.onToast(`Grupo "${g.name}" criado!`, "groups");
        await this.refreshState();
      } catch (e) {
        this.onToast(String(e), "error");
      } finally {
        this.btnConfirmCreateGroup.disabled = false;
      }
    };

    this.btnConfirmCreateGroup.addEventListener("click", handleCreateGroup);
    this.createGroupNameInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleCreateGroup();
    });

    const handleJoinGroup = async () => {
      const code = this.joinGroupCodeInput.value.trim();
      if (!code) {
        this.onToast("Cole o código LKG-XXXX-XXXX ou ticket lkg-... do grupo.", "vpn_key");
        this.joinGroupCodeInput.focus();
        return;
      }
      this.btnConfirmJoinGroup.disabled = true;
      try {
        const g = await invoke<GroupView>("join_group_by_code", { code });
        this.joinGroupCodeInput.value = "";
        this.closeModal(this.groupModal);
        this.onModeChange("group");
        this.onToast(`Você entrou no grupo "${g.name}"!`, "group_add");
        await this.refreshState();
      } catch (e) {
        this.onToast(String(e), "error");
      } finally {
        this.btnConfirmJoinGroup.disabled = false;
      }
    };

    this.btnConfirmJoinGroup.addEventListener("click", handleJoinGroup);
    this.joinGroupCodeInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleJoinGroup();
    });

    // Modal Nova Sala
    this.btnCreateRoom.addEventListener("click", () => {
      if (!this.state.active_group_id) return;
      this.openModal(this.roomModal, this.btnCreateRoom);
      this.newRoomNameInput.focus();
    });
    this.btnCloseRoomModal.addEventListener("click", () => this.closeModal(this.roomModal));
    this.roomModal.addEventListener("click", (e) => {
      if (e.target === this.roomModal) this.closeModal(this.roomModal);
    });

    const handleCreateRoom = async () => {
      const gid = this.state.active_group_id;
      const name = this.newRoomNameInput.value.trim();
      if (!gid || !name) return;
      this.btnConfirmCreateRoom.disabled = true;
      try {
        await invoke("create_group_room", { groupId: gid, name });
        this.newRoomNameInput.value = "";
        this.closeModal(this.roomModal);
        this.onToast(`Sala #${name} criada no grupo!`, "cast");
        await this.refreshState();
      } catch (e) {
        this.onToast(String(e), "error");
      } finally {
        this.btnConfirmCreateRoom.disabled = false;
      }
    };
    this.btnConfirmCreateRoom.addEventListener("click", handleCreateRoom);
    this.newRoomNameInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") handleCreateRoom();
    });

    // Modal Convidar para Grupo
    this.btnInviteGroup.addEventListener("click", () => this.openInviteModal());
    this.btnCloseInviteModal.addEventListener("click", () => this.closeModal(this.inviteModal));
    this.inviteModal.addEventListener("click", (e) => {
      if (e.target === this.inviteModal) this.closeModal(this.inviteModal);
    });

    this.btnCopyInviteCode.addEventListener("click", () => {
      const v = this.inviteCodeDisplay.value.trim();
      if (!v) return;
      navigator.clipboard.writeText(v);
      this.onToast(`Código ${v} copiado!`, "content_copy");
    });

    this.btnCopyInviteTicket.addEventListener("click", () => {
      const v = this.inviteTicketDisplay.value.trim();
      if (!v) return;
      navigator.clipboard.writeText(v);
      this.onToast("Ticket completo do grupo copiado!", "content_copy");
    });

    this.btnLeaveGroup.addEventListener("click", async () => {
      const gid = this.state.active_group_id;
      if (!gid) return;
      const grp = this.state.groups.find((g) => g.id === gid);
      if (!grp) return;
      try {
        await invoke("leave_group", { groupId: gid });
        this.onToast(`Você saiu de "${grp.name}"`, "logout");
        await this.refreshState();
      } catch (e) {
        this.onToast(String(e), "error");
      }
    });

    this.btnCollapseSidebar.addEventListener("click", () => {
      this.isSidebarCollapsed = !this.isSidebarCollapsed;
      this.groupSidebar.classList.toggle("collapsed", this.isSidebarCollapsed);
    });

    this.btnExitRoom.addEventListener("click", async () => {
      await this.exitCurrentRoom();
    });

    this.btnQuickScreen.addEventListener("click", () => this.onRequestScreenShare());
    this.btnQuickCam.addEventListener("click", () => this.onRequestWebcamToggle());

    if (this.autoWatchCheckbox) {
      this.autoWatchCheckbox.checked = this.autoWatch;
      this.autoWatchCheckbox.addEventListener("change", () => {
        const next = Boolean(this.autoWatchCheckbox?.checked);
        const currentAvailable = this.getCurrentRoomAvailableStreams();
        if (!next && this.autoWatch) {
          this.manualTickets.clear();
          for (const s of currentAvailable) {
            if (!this.dismissedTickets.has(s.ticket)) {
              this.manualTickets.add(s.ticket);
            }
          }
          this.dismissedTickets.clear();
          this.onToast("Modo econômico: clique em qual live quer assistir", "tune");
        } else if (next && !this.autoWatch) {
          this.dismissedTickets.clear();
          this.manualTickets.clear();
          this.onToast("Lives da sala abrirão automaticamente ao entrar", "visibility");
        }
        this.autoWatch = next;
        localStorage.setItem("luke_group_auto_watch", next ? "true" : "false");
        this.syncDesiredStreams();
        this.render();
      });
    }

    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        if (!this.groupModal.classList.contains("hidden")) this.closeModal(this.groupModal);
        if (!this.roomModal.classList.contains("hidden")) this.closeModal(this.roomModal);
        if (!this.inviteModal.classList.contains("hidden")) this.closeModal(this.inviteModal);
      }
    });
  }

  private openModal(modal: HTMLElement, opener?: HTMLElement | null) {
    this.lastOpener = opener || (document.activeElement as HTMLElement | null);
    modal.classList.remove("hidden");
  }

  private closeModal(modal: HTMLElement) {
    modal.classList.add("hidden");
    if (this.lastOpener && typeof this.lastOpener.focus === "function") {
      this.lastOpener.focus();
      this.lastOpener = null;
    }
  }

  public async switchToPrivate() {
    if (this.state.active_room_id) {
      await invoke("exit_group_room").catch(() => {});
    }
    await invoke("select_group", { groupId: null }).catch(() => {});
    this.onModeChange("private");
    await this.refreshState();
  }

  public async selectGroup(groupId: string) {
    this.isSidebarCollapsed = false;
    this.groupSidebar.classList.remove("collapsed");
    await invoke("select_group", { groupId }).catch(() => {});
    this.onModeChange("group");
    await this.refreshState();
  }

  public async enterRoom(groupId: string, roomId: string) {
    try {
      await invoke("enter_group_room", { groupId, roomId });
      this.onModeChange("group");
      await this.refreshState();
    } catch (e) {
      this.onToast(String(e), "error");
    }
  }

  public async exitCurrentRoom() {
    try {
      await invoke("exit_group_room");
      await this.refreshState();
      this.onToast("Você saiu da sala de streams", "call_end");
    } catch (e) {
      this.onToast(String(e), "error");
    }
  }

  private async openInviteModal() {
    const gid = this.state.active_group_id;
    if (!gid) return;
    this.openModal(this.inviteModal, this.btnInviteGroup);
    this.inviteCodeDisplay.value = "Gerando código...";
    this.inviteTicketDisplay.value = "Gerando ticket...";

    try {
      const bundle = await invoke<GroupInviteBundle>("create_group_invite", { groupId: gid });
      this.inviteCodeDisplay.value = bundle.short_code || "Indisponível";
      this.inviteTicketDisplay.value = bundle.direct_ticket;
    } catch (e) {
      this.inviteCodeDisplay.value = "Erro";
      this.inviteTicketDisplay.value = "";
      this.onToast("Erro gerando convite: " + e, "error");
    }

    // Lista amigos disponíveis para convidar direto por P2P
    try {
      const friends = await invoke<Friend[]>("list_friends");
      const accepted = (friends || []).filter((f) => f.status === "Accepted");
      this.inviteFriendsList.innerHTML = "";

      if (accepted.length === 0) {
        this.inviteFriendsList.innerHTML = `<div class="invite-empty">Nenhum amigo adicionado ainda. Compartilhe o código LKG acima!</div>`;
        return;
      }

      for (const f of accepted) {
        const row = document.createElement("div");
        row.className = "invite-friend-row";
        row.innerHTML = `
          <div class="invite-friend-meta">
            <span class="friend-presence-dot ${f.is_online ? "online" : "offline"}"></span>
            <span class="invite-friend-nick">${this.escapeHtml(f.nick)}</span>
            <span class="invite-friend-tag">${this.escapeHtml(f.tag)}</span>
          </div>
          <button type="button" class="btn-secondary btn-xs btn-send-group-invite">
            <span>${f.is_online ? "Convidar" : "Offline"}</span>
          </button>
        `;
        const btn = row.querySelector(".btn-send-group-invite") as HTMLButtonElement;
        btn.disabled = !f.is_online;
        btn.addEventListener("click", async () => {
          btn.disabled = true;
          btn.textContent = "Enviando...";
          try {
            await invoke("invite_friend_to_group", { groupId: gid, friendTag: f.tag });
            btn.textContent = "Enviado";
            this.onToast(`Convite enviado para ${f.nick}!`, "send");
          } catch (e) {
            btn.disabled = false;
            btn.textContent = "Convidar";
            this.onToast(String(e), "error");
          }
        });
        this.inviteFriendsList.appendChild(row);
      }
    } catch (_) {}
  }

  public async refreshState() {
    try {
      const snap = await invoke<GroupsStateSnapshot>("get_groups_state");
      if (!snap) return;
      this.state = snap;
      this.render();
      this.syncDesiredStreams();
    } catch (_) {}
  }

  public isStreamWanted(ticket: string): boolean {
    return this.autoWatch
      ? !this.dismissedTickets.has(ticket)
      : this.manualTickets.has(ticket);
  }

  public setStreamWanted(stream: DesiredGroupStream, wanted: boolean) {
    if (this.autoWatch) {
      if (wanted) this.dismissedTickets.delete(stream.ticket);
      else this.dismissedTickets.add(stream.ticket);
    } else {
      if (wanted) this.manualTickets.add(stream.ticket);
      else this.manualTickets.delete(stream.ticket);
    }
    this.syncDesiredStreams();
    this.render();
  }

  public markStreamStopped(ticket: string) {
    if (this.autoWatch) this.dismissedTickets.add(ticket);
    else this.manualTickets.delete(ticket);
    this.render();
  }

  public watchAllAvailable() {
    this.dismissedTickets.clear();
    for (const s of this.getCurrentRoomAvailableStreams()) {
      this.manualTickets.add(s.ticket);
    }
    this.syncDesiredStreams();
    this.render();
  }

  public getCurrentRoomAvailableStreams(): DesiredGroupStream[] {
    const gid = this.state.active_group_id;
    const rid = this.state.active_room_id;
    if (!gid || !rid) return [];

    const grp = this.state.groups.find((g) => g.id === gid);
    const room = grp?.rooms.find((r) => r.id === rid);
    if (!grp || !room) return [];

    const available: DesiredGroupStream[] = [];
    for (const p of room.participants) {
      if (p.is_self) continue;
      if (p.has_screen && p.screen_ticket) {
        available.push({
          streamKey: `grp:${p.tag}:screen`,
          ticket: p.screen_ticket,
          nick: `${p.nick} — Tela`,
          kind: "screen",
        });
      }
      if (p.has_camera && p.camera_ticket) {
        available.push({
          streamKey: `grp:${p.tag}:camera`,
          ticket: p.camera_ticket,
          nick: `${p.nick} — Webcam`,
          kind: "camera",
        });
      }
    }
    return available;
  }

  private syncDesiredStreams() {
    const gid = this.state.active_group_id;
    const rid = this.state.active_room_id;
    if (!gid || !rid) {
      this.lastRoomKey = null;
      this.dismissedTickets.clear();
      this.manualTickets.clear();
      this.onDesiredRoomStreams(false, null, null, [], []);
      return;
    }

    const grp = this.state.groups.find((g) => g.id === gid);
    const room = grp?.rooms.find((r) => r.id === rid);
    if (!grp || !room) {
      this.lastRoomKey = null;
      this.onDesiredRoomStreams(false, null, null, [], []);
      return;
    }

    const roomKey = `${gid}:${rid}`;
    if (roomKey !== this.lastRoomKey) {
      this.lastRoomKey = roomKey;
      this.dismissedTickets.clear();
      this.manualTickets.clear();
    }

    const available = this.getCurrentRoomAvailableStreams();
    const activeTickets = new Set(available.map((a) => a.ticket));
    for (const t of Array.from(this.dismissedTickets)) {
      if (!activeTickets.has(t)) this.dismissedTickets.delete(t);
    }
    for (const t of Array.from(this.manualTickets)) {
      if (!activeTickets.has(t)) this.manualTickets.delete(t);
    }

    const desired = available.filter((d) => this.isStreamWanted(d.ticket));
    this.onDesiredRoomStreams(true, grp.name, room.name, desired, available);
  }

  private renderPendingInvites() {
    if (!this.pendingInvitesArea) return;
    const list = this.state.pending_invites || [];
    if (list.length === 0) {
      this.pendingInvitesArea.classList.add("hidden");
      this.pendingInvitesArea.innerHTML = "";
      return;
    }
    this.pendingInvitesArea.classList.remove("hidden");
    this.pendingInvitesArea.innerHTML = `
      <div class="pending-invites-title">Convites de Grupo Pendentes</div>
      ${list
        .map(
          (inv) => `
        <div class="pending-invite-card" data-gid="${this.escapeHtml(inv.group_id)}">
          <div class="pending-invite-info">
            <strong>${this.escapeHtml(inv.group_name)}</strong>
            <span>Por ${this.escapeHtml(inv.from_nick)} (${inv.members_count} membros)</span>
          </div>
          <div class="pending-invite-actions">
            <button type="button" class="btn-secondary btn-xs btn-decline-inv">Recusar</button>
            <button type="button" class="btn-primary btn-xs btn-accept-inv">Entrar</button>
          </div>
        </div>
      `
        )
        .join("")}
    `;

    this.pendingInvitesArea.querySelectorAll<HTMLElement>(".pending-invite-card").forEach((card) => {
      const gid = card.dataset.gid!;
      card.querySelector(".btn-accept-inv")?.addEventListener("click", async () => {
        await invoke("respond_group_invite", { groupId: gid, accept: true });
        this.closeModal(this.groupModal);
        this.onModeChange("group");
        await this.refreshState();
      });
      card.querySelector(".btn-decline-inv")?.addEventListener("click", async () => {
        await invoke("respond_group_invite", { groupId: gid, accept: false });
        await this.refreshState();
      });
    });
  }

  private render() {
    const activeGid = this.state.active_group_id;
    const activeRid = this.state.active_room_id;

    // 1. Rail: Privado vs Grupos
    const inPrivate = !activeGid;
    this.btnNavPrivate.classList.toggle("active", inPrivate);
    this.btnNavPrivate.setAttribute("aria-current", inPrivate ? "page" : "false");

    const invCount = this.state.pending_invites?.length || 0;
    if (this.railInviteBadge) {
      this.railInviteBadge.textContent = String(invCount);
      this.railInviteBadge.classList.toggle("hidden", invCount === 0);
    }

    this.railList.innerHTML = "";
    for (const grp of this.state.groups) {
      const isSelected = grp.id === activeGid;
      const liveParticipants = grp.rooms.reduce((acc, r) => acc + r.participants.length, 0);
      const initials = grp.name
        .split(/\s+/)
        .slice(0, 2)
        .map((w) => w[0]?.toUpperCase() || "")
        .join("") || "G";

      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = `rail-item ${isSelected ? "active" : ""}`;
      btn.setAttribute("aria-label", `Grupo ${grp.name}${liveParticipants > 0 ? `, ${liveParticipants} na sala` : ""}`);
      btn.setAttribute("aria-current", isSelected ? "page" : "false");
      btn.title = `${grp.name} (${grp.online_members_count}/${grp.members_count} online)`;
      btn.innerHTML = `
        <span class="rail-indicator"></span>
        <span class="rail-avatar">${this.escapeHtml(initials)}</span>
        ${liveParticipants > 0 ? `<span class="rail-live-dot" title="${liveParticipants} na sala"></span>` : ""}
      `;
      btn.addEventListener("click", () => this.selectGroup(grp.id));
      this.railList.appendChild(btn);
    }

    // 2. Sidebar do Grupo selecionado
    const activeGrp = activeGid ? this.state.groups.find((g) => g.id === activeGid) : undefined;
    if (!activeGrp) {
      this.groupSidebar.classList.add("hidden");
      return;
    }

    this.groupSidebar.classList.remove("hidden");
    this.groupTitleEl.textContent = activeGrp.name;
    this.groupMetaEl.textContent = `${activeGrp.online_members_count}/${activeGrp.members_count} membros • Sem voz`;

    this.roomsListEl.innerHTML = "";
    for (const room of activeGrp.rooms) {
      const isJoined = activeRid === room.id;
      const roomBlock = document.createElement("div");
      roomBlock.className = "group-room-block";

      const roomBtn = document.createElement("button");
      roomBtn.type = "button";
      roomBtn.className = `group-room-btn ${isJoined ? "joined" : ""}`;
      roomBtn.setAttribute("aria-pressed", String(isJoined));
      roomBtn.innerHTML = `
        <span class="material-symbols-rounded ms-sm room-icon">cast</span>
        <span class="room-name">${this.escapeHtml(room.name)}</span>
        ${
          room.participants.length > 0
            ? `<span class="room-count-pill">${room.participants.length}</span>`
            : ""
        }
      `;
      roomBtn.addEventListener("click", () => {
        if (!isJoined) {
          this.enterRoom(activeGrp.id, room.id);
        }
      });
      roomBlock.appendChild(roomBtn);

      if (room.participants.length > 0) {
        const plist = document.createElement("div");
        plist.className = "room-participants-list";
        for (const p of room.participants) {
          const prow = document.createElement("div");
          prow.className = `room-participant-row ${p.is_self ? "is-self" : ""}`;

          const trackBtns: { stream: DesiredGroupStream; watching: boolean }[] = [];
          const chips: string[] = [];
          if (p.has_screen) {
            if (isJoined && !p.is_self && p.screen_ticket) {
              const streamObj: DesiredGroupStream = {
                streamKey: `grp:${p.tag}:screen`,
                ticket: p.screen_ticket,
                nick: `${p.nick} — Tela`,
                kind: "screen",
              };
              const watching = this.isStreamWanted(p.screen_ticket);
              trackBtns.push({ stream: streamObj, watching });
              chips.push(
                `<button type="button" class="participant-track-chip screen interactive ${
                  watching ? "watching" : "paused"
                }" data-track-idx="${trackBtns.length - 1}" aria-pressed="${watching}" title="${
                  watching
                    ? "Assistindo — clique para parar de assistir esta tela"
                    : "Pausada — clique para assistir esta tela"
                }"><span class="material-symbols-rounded ms-xs">${
                  watching ? "visibility" : "play_arrow"
                }</span><span>${watching ? "Tela" : "Ver Tela"}</span></button>`
              );
            } else {
              chips.push(
                `<span class="participant-track-chip screen"><span class="material-symbols-rounded ms-xs">desktop_windows</span><span>Tela</span></span>`
              );
            }
          }
          if (p.has_camera) {
            if (isJoined && !p.is_self && p.camera_ticket) {
              const streamObj: DesiredGroupStream = {
                streamKey: `grp:${p.tag}:camera`,
                ticket: p.camera_ticket,
                nick: `${p.nick} — Webcam`,
                kind: "camera",
              };
              const watching = this.isStreamWanted(p.camera_ticket);
              trackBtns.push({ stream: streamObj, watching });
              chips.push(
                `<button type="button" class="participant-track-chip cam interactive ${
                  watching ? "watching" : "paused"
                }" data-track-idx="${trackBtns.length - 1}" aria-pressed="${watching}" title="${
                  watching
                    ? "Assistindo — clique para parar de assistir esta webcam"
                    : "Pausada — clique para assistir esta webcam"
                }"><span class="material-symbols-rounded ms-xs">${
                  watching ? "videocam" : "play_arrow"
                }</span><span>${watching ? "Cam" : "Ver Cam"}</span></button>`
              );
            } else {
              chips.push(
                `<span class="participant-track-chip cam"><span class="material-symbols-rounded ms-xs">videocam</span><span>Cam</span></span>`
              );
            }
          }
          if (chips.length === 0) {
            chips.push(`<span class="participant-track-chip idle">Na sala</span>`);
          }

          prow.innerHTML = `
            <span class="participant-dot ${p.has_screen || p.has_camera ? "live" : ""}"></span>
            <span class="participant-nick">${this.escapeHtml(p.nick)}${p.is_self ? " (Você)" : ""}</span>
            <div class="participant-chips">${chips.join("")}</div>
          `;
          if (trackBtns.length > 0) {
            prow.querySelectorAll<HTMLButtonElement>(".participant-track-chip.interactive").forEach((btn) => {
              const idx = parseInt(btn.dataset.trackIdx || "-1", 10);
              const entry = trackBtns[idx];
              if (!entry) return;
              btn.addEventListener("click", (e) => {
                e.stopPropagation();
                const nextWanted = !entry.watching;
                this.setStreamWanted(entry.stream, nextWanted);
                this.onToast(
                  nextWanted
                    ? `Abrindo ${entry.stream.nick}`
                    : `Parou de assistir ${entry.stream.nick}`,
                  nextWanted ? "visibility" : "visibility_off"
                );
              });
            });
          }
          plist.appendChild(prow);
        }
        roomBlock.appendChild(plist);
      }

      this.roomsListEl.appendChild(roomBlock);
    }

    // Rodapé conectado da sala
    const activeRoom = activeRid ? activeGrp.rooms.find((r) => r.id === activeRid) : undefined;
    if (activeRoom) {
      this.roomFooterEl.classList.remove("hidden");
      this.roomFooterTitle.textContent = `# ${activeRoom.name}`;
    } else {
      this.roomFooterEl.classList.add("hidden");
    }

    if (!this.groupModal.classList.contains("hidden")) {
      this.renderPendingInvites();
    }
  }

  private escapeHtml(str: string): string {
    return str.replace(/[&<>'"]/g, (c) => `&#${c.charCodeAt(0)};`);
  }

  public destroy() {
    if (this.pollTimer !== null) {
      clearInterval(this.pollTimer);
    }
  }
}
