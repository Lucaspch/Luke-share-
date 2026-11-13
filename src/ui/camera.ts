import { invoke, Channel } from "@tauri-apps/api/core";
import {
  StreamEvent,
  StreamChannelMsg,
  extractBinaryBuffer,
} from "../media/player";
import { ShareInfo } from "./share";

export interface CameraDeviceInfo {
  id: string;
  name: string;
  width: number;
  height: number;
  fps: number;
}

export interface CameraUiRefs {
  modalBackdrop: HTMLElement;
  btnClose: HTMLButtonElement;
  btnCancel: HTMLButtonElement;
  btnStart: HTMLButtonElement;
  btnRefresh: HTMLButtonElement;
  deviceSelect: HTMLSelectElement;
  resolutionSelect: HTMLSelectElement;
  fpsSelect: HTMLSelectElement;
  mirrorCheckbox: HTMLInputElement;
  emptyNotice: HTMLElement;
}

export class CameraManager {
  private refs: CameraUiRefs;
  private cameras: CameraDeviceInfo[] = [];
  private isSharing = false;
  private isMirrored = true;
  private shareInfo: ShareInfo | null = null;
  private lastOpener: HTMLElement | null = null;

  private onStateChange: (active: boolean, mirrored: boolean) => void;
  private onPreviewPacket: (msg: StreamChannelMsg) => void;
  private onToast: (msg: string, icon?: string) => void;

  constructor(
    refs: CameraUiRefs,
    onStateChange: (active: boolean, mirrored: boolean) => void,
    onPreviewPacket: (msg: StreamChannelMsg) => void,
    onToast: (msg: string, icon?: string) => void
  ) {
    this.refs = refs;
    this.onStateChange = onStateChange;
    this.onPreviewPacket = onPreviewPacket;
    this.onToast = onToast;

    this.bindEvents();
  }

  private bindEvents() {
    this.refs.btnClose.addEventListener("click", () => this.closeModal());
    this.refs.btnCancel.addEventListener("click", () => this.closeModal());
    this.refs.modalBackdrop.addEventListener("click", (e) => {
      if (e.target === this.refs.modalBackdrop) this.closeModal();
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && !this.refs.modalBackdrop.classList.contains("hidden")) {
        this.closeModal();
      }
    });

    this.refs.btnRefresh.addEventListener("click", () => {
      this.refreshCameras();
    });

    this.refs.mirrorCheckbox.addEventListener("change", () => {
      this.isMirrored = this.refs.mirrorCheckbox.checked;
      if (this.isSharing) {
        this.onStateChange(true, this.isMirrored);
      }
    });

    this.refs.btnStart.addEventListener("click", () => {
      this.startSelectedCamera();
    });
  }

  public getIsSharing(): boolean {
    return this.isSharing;
  }

  public getIsMirrored(): boolean {
    return this.isMirrored;
  }

  public getShareInfo(): ShareInfo | null {
    return this.shareInfo;
  }

  public async openModal(opener?: HTMLElement | null) {
    this.lastOpener = opener || (document.activeElement as HTMLElement | null);
    this.refs.mirrorCheckbox.checked = this.isMirrored;
    this.refs.modalBackdrop.classList.remove("hidden");
    await this.refreshCameras();
    this.refs.deviceSelect.focus();
  }

  public closeModal() {
    this.refs.modalBackdrop.classList.add("hidden");
    if (this.lastOpener && typeof this.lastOpener.focus === "function") {
      this.lastOpener.focus();
      this.lastOpener = null;
    }
  }

  public async refreshCameras() {
    this.refs.btnRefresh.disabled = true;
    try {
      const list = await invoke<CameraDeviceInfo[]>("list_cameras");
      this.cameras = list || [];
      this.refs.deviceSelect.innerHTML = "";

      if (this.cameras.length === 0) {
        this.refs.emptyNotice.classList.remove("hidden");
        const opt = document.createElement("option");
        opt.value = "";
        opt.textContent = "Nenhuma câmera detectada";
        this.refs.deviceSelect.appendChild(opt);
        this.refs.deviceSelect.disabled = true;
        this.refs.btnStart.disabled = true;
      } else {
        this.refs.emptyNotice.classList.add("hidden");
        this.refs.deviceSelect.disabled = false;
        this.refs.btnStart.disabled = false;
        for (const cam of this.cameras) {
          const opt = document.createElement("option");
          opt.value = cam.id;
          opt.textContent = `${cam.name} (${cam.width}x${cam.height} @ ${cam.fps}fps)`;
          this.refs.deviceSelect.appendChild(opt);
        }
      }
    } catch (e) {
      this.onToast("Erro ao listar webcams: " + e, "error");
    } finally {
      this.refs.btnRefresh.disabled = false;
    }
  }

  public async startSelectedCamera(publishShortCode = false) {
    const deviceId = this.refs.deviceSelect.value;
    if (!deviceId) {
      this.onToast("Selecione uma webcam conectada antes de iniciar.", "videocam_off");
      return;
    }

    const presetStr = this.refs.resolutionSelect.value || "720p";
    const fpsTarget = parseInt(this.refs.fpsSelect.value || "30", 10) || 30;
    this.isMirrored = this.refs.mirrorCheckbox.checked;

    this.refs.btnStart.disabled = true;
    const prevText = this.refs.btnStart.innerHTML;
    this.refs.btnStart.innerHTML = `<span class="material-symbols-rounded ms-sm">hourglass_top</span><span>Conectando...</span>`;

    const onPreview = new Channel<StreamChannelMsg>();
    onPreview.onmessage = (msg) => {
      const bin = extractBinaryBuffer(msg);
      if (bin) {
        this.onPreviewPacket(bin);
        return;
      }
      const ev = msg as StreamEvent;
      if (ev.t === "config" || ev.t === "audio_state") {
        this.onPreviewPacket(ev);
      }
    };

    try {
      const info = await invoke<ShareInfo>("start_camera_share", {
        deviceId,
        presetStr,
        fpsTarget,
        bitrateKbps: presetStr === "1080" ? 4500 : 2800,
        codec: "auto",
        publishShortCode,
        onPreview,
      });

      this.isSharing = true;
      this.shareInfo = info;
      this.closeModal();
      this.onStateChange(true, this.isMirrored);
      this.onToast(
        publishShortCode && info.short_code
          ? `Webcam ao vivo (${info.short_code})`
          : "Webcam ativada na transmissão (sem microfone)",
        "videocam"
      );
    } catch (e) {
      this.onToast("Falha ao abrir webcam: " + e, "error");
    } finally {
      this.refs.btnStart.disabled = false;
      this.refs.btnStart.innerHTML = prevText;
    }
  }

  public async stopCamera() {
    if (!this.isSharing) return;
    try {
      await invoke("stop_camera_share");
    } catch (e) {
      console.warn("Erro ao parar webcam:", e);
    } finally {
      this.isSharing = false;
      this.shareInfo = null;
      this.onStateChange(false, this.isMirrored);
      this.onToast("Webcam desligada", "videocam_off");
    }
  }
}
