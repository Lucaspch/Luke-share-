use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    pub current_version: String,
    pub version: Option<String>,
    pub date: Option<String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    pub chunk_length: usize,
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// Checa se há atualização disponível no repositório configurado
#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<UpdateInfo, String> {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => return Err(format!("Erro ao inicializar updater: {}", e)),
    };

    let update = match updater.check().await {
        Ok(u) => u,
        Err(e) => return Err(format!("Falha ao verificar atualização: {}", e)),
    };

    match update {
        Some(u) => Ok(UpdateInfo {
            available: true,
            current_version: u.current_version.clone(),
            version: Some(u.version.clone()),
            date: u.date.as_ref().map(|d| d.to_string()),
            body: u.body.clone(),
        }),
        None => {
            let app_version = app.package_info().version.to_string();
            Ok(UpdateInfo {
                available: false,
                current_version: app_version,
                version: None,
                date: None,
                body: None,
            })
        }
    }
}

/// Baixa e instala a atualização mais recente, emitindo eventos de progresso
#[tauri::command]
pub async fn download_and_install_update(app: AppHandle) -> Result<(), String> {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => return Err(format!("Erro ao inicializar updater: {}", e)),
    };

    let update = match updater.check().await {
        Ok(Some(u)) => u,
        Ok(None) => return Err("Nenhuma atualização disponível para instalar.".into()),
        Err(e) => return Err(format!("Falha ao verificar atualização: {}", e)),
    };

    let app_handle = app.clone();
    let mut downloaded: u64 = 0;

    let res = update
        .download_and_install(
            move |chunk_length, content_length| {
                downloaded += chunk_length as u64;
                let _ = app_handle.emit(
                    "updater://progress",
                    DownloadProgress {
                        chunk_length,
                        downloaded,
                        total: content_length,
                    },
                );
            },
            || {
                // Download concluído, iniciando descompressão / instalação
            },
        )
        .await;

    match res {
        Ok(_) => {
            let _ = app.emit("updater://installed", ());
            Ok(())
        }
        Err(e) => Err(format!("Falha ao baixar/instalar atualização: {}", e)),
    }
}

/// Reinicia a aplicação para aplicar a atualização instalada
#[tauri::command]
pub fn restart_app(app: AppHandle) {
    app.restart();
}
