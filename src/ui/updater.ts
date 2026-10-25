import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";

export interface UpdateInfo {
  available: boolean;
  current_version: string;
  version: string | null;
  date: string | null;
  body: string | null;
}

export interface DownloadProgress {
  chunk_length: number;
  downloaded: number;
  total: number | null;
}

export class UpdaterManager {
  private btnCheckUpdates: HTMLButtonElement | null = null;
  private updateModal: HTMLElement | null = null;
  private updateBadge: HTMLElement | null = null;
  private isUpdating = false;

  constructor() {
    this.initDOMElements();
    this.syncHeaderVersion();
    this.bindEvents();
    this.listenToProgressEvents();

    // Verificação automática silenciosa após 3.5 segundos da inicialização
    setTimeout(() => {
      this.checkForUpdates(false).catch((e) => {
        console.debug("Verificação automática de update (esperado caso offline):", e);
      });
    }, 3500);
  }

  private async syncHeaderVersion() {
    try {
      const ver = await getVersion();
      const tag = document.getElementById("header-version-tag");
      if (tag && ver) {
        tag.textContent = `v${ver}`;
      }
    } catch (_) {}
  }

  private initDOMElements() {
    this.btnCheckUpdates = document.getElementById("btn-check-updates") as HTMLButtonElement | null;
    this.updateModal = document.getElementById("update-modal");
    this.updateBadge = document.getElementById("update-badge");
  }

  private bindEvents() {
    if (this.btnCheckUpdates) {
      this.btnCheckUpdates.addEventListener("click", () => {
        this.checkForUpdates(true);
      });
    }

    const btnCloseModal = document.getElementById("btn-close-update-modal");
    if (btnCloseModal) {
      btnCloseModal.addEventListener("click", () => {
        if (!this.isUpdating) {
          this.closeModal();
        }
      });
    }

    const btnStartUpdate = document.getElementById("btn-start-update");
    if (btnStartUpdate) {
      btnStartUpdate.addEventListener("click", () => {
        this.startDownloadAndInstall();
      });
    }
  }

  private async listenToProgressEvents() {
    try {
      await listen<DownloadProgress>("updater://progress", (event) => {
        const payload = event.payload;
        this.onProgress(payload);
      });

      await listen("updater://installed", () => {
        this.onInstalled();
      });
    } catch (err) {
      console.warn("Falha ao registrar listeners de eventos do updater:", err);
    }
  }

  public async checkForUpdates(manual: boolean): Promise<void> {
    if (this.isUpdating) return;

    if (manual && this.btnCheckUpdates) {
      this.btnCheckUpdates.disabled = true;
      this.btnCheckUpdates.innerHTML = `<span class="material-symbols-rounded ms-sm">hourglass_top</span><span>Checando...</span>`;
    }

    try {
      const info = await invoke<UpdateInfo>("check_for_updates");

      if (info.available && info.version) {
        this.showUpdateAvailable(info);
      } else {
        if (manual) {
          this.showUpToDateNotice(info.current_version);
        }
      }
    } catch (err: any) {
      console.error("Erro na verificação de atualização:", err);
      if (manual) {
        alert(
          `Não foi possível verificar atualizações no momento.\nDetalhes: ${
            err?.message || err || "Erro de rede"
          }`
        );
      }
    } finally {
      if (manual && this.btnCheckUpdates) {
        this.btnCheckUpdates.disabled = false;
        this.btnCheckUpdates.innerHTML = `<span class="material-symbols-rounded ms-sm">sync</span><span>Atualizações</span><span id="update-badge" class="update-badge-dot hidden"></span>`;
      }
    }
  }

  private showUpdateAvailable(info: UpdateInfo) {
    if (this.updateBadge) {
      this.updateBadge.classList.remove("hidden");
    }

    const newVersionEl = document.getElementById("update-new-version");
    const currentVersionEl = document.getElementById("update-curr-version");
    const notesEl = document.getElementById("update-notes");
    const progressContainer = document.getElementById("update-progress-container");
    const actionButtons = document.getElementById("update-action-buttons");

    if (newVersionEl) newVersionEl.textContent = `v${info.version}`;
    if (currentVersionEl) currentVersionEl.textContent = `v${info.current_version}`;

    if (notesEl) {
      notesEl.textContent = info.body || "Correções e melhorias de estabilidade.";
    }

    if (progressContainer) progressContainer.classList.add("hidden");
    if (actionButtons) actionButtons.classList.remove("hidden");

    this.openModal();
  }

  private showUpToDateNotice(currentVersion: string) {
    alert(`O Luke-Share já está na versão mais recente (v${currentVersion})!`);
  }

  private async startDownloadAndInstall() {
    if (this.isUpdating) return;
    this.isUpdating = true;

    const actionButtons = document.getElementById("update-action-buttons");
    const progressContainer = document.getElementById("update-progress-container");
    const statusText = document.getElementById("update-status-text");

    if (actionButtons) actionButtons.classList.add("hidden");
    if (progressContainer) progressContainer.classList.remove("hidden");
    if (statusText) statusText.textContent = "Baixando atualização do Luke-Share...";

    try {
      await invoke("download_and_install_update");
    } catch (err: any) {
      this.isUpdating = false;
      console.error("Falha ao instalar atualização:", err);
      if (statusText) {
        statusText.textContent = `Erro ao instalar: ${err?.message || err}`;
        statusText.style.color = "#ef4444";
      }
      if (actionButtons) actionButtons.classList.remove("hidden");
    }
  }

  private onProgress(progress: DownloadProgress) {
    const progressBar = document.getElementById("update-progress-bar") as HTMLElement | null;
    const progressText = document.getElementById("update-progress-text");

    if (progress.total && progress.total > 0) {
      const pct = Math.min(100, Math.round((progress.downloaded / progress.total) * 100));
      if (progressBar) progressBar.style.width = `${pct}%`;

      const downloadedMb = (progress.downloaded / (1024 * 1024)).toFixed(1);
      const totalMb = (progress.total / (1024 * 1024)).toFixed(1);
      if (progressText) {
        progressText.textContent = `${downloadedMb} MB de ${totalMb} MB (${pct}%)`;
      }
    } else {
      const downloadedMb = (progress.downloaded / (1024 * 1024)).toFixed(1);
      if (progressBar) progressBar.style.width = `75%`;
      if (progressText) {
        progressText.textContent = `${downloadedMb} MB baixados...`;
      }
    }
  }

  private onInstalled() {
    const statusText = document.getElementById("update-status-text");
    const progressBar = document.getElementById("update-progress-bar") as HTMLElement | null;
    const progressText = document.getElementById("update-progress-text");

    if (progressBar) progressBar.style.width = `100%`;
    if (statusText) statusText.textContent = "Atualização instalada com sucesso!";
    if (progressText) progressText.textContent = "Reiniciando aplicação em 2 segundos...";

    setTimeout(() => {
      invoke("restart_app").catch(() => {
        window.location.reload();
      });
    }, 2000);
  }

  private openModal() {
    if (this.updateModal) {
      this.updateModal.classList.remove("hidden");
    }
  }

  private closeModal() {
    if (this.updateModal) {
      this.updateModal.classList.add("hidden");
    }
  }
}
