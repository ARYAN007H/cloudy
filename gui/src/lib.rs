use clouddrive_core::{
    AppConfig, CoreEngine, SyncStatus, TransferInfo, TransferStatus, UploadPolicy,
    VirtualNode,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::State;
use tokio::sync::RwLock;


pub struct AppState {
    pub engine: Arc<RwLock<Option<CoreEngine>>>,
}

// ─── DTOs ────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct VirtualNodeDto {
    pub id: String,
    pub drive_file_id: Option<String>,
    pub account: Option<String>,
    pub name: String,
    pub display_name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mime_type: String,
    pub modified_time: String,
    pub is_read_only: bool,
    pub children_ids: Vec<String>,
    pub is_workspace_file: bool,
    pub thumbnail_link: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ClientCredentialsInfoDto {
    pub has_credentials: bool,
    pub client_id: String,
}

impl From<VirtualNode> for VirtualNodeDto {
    fn from(n: VirtualNode) -> Self {
        Self {
            id: n.id,
            drive_file_id: n.drive_file_id,
            account: n.account,
            name: n.name,
            display_name: n.display_name,
            is_dir: n.is_dir,
            size: n.size,
            mime_type: n.mime_type,
            modified_time: n.modified_time.to_rfc3339(),
            is_read_only: n.is_read_only,
            children_ids: n.children_ids,
            is_workspace_file: n.is_workspace_file,
            thumbnail_link: n.thumbnail_link,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AccountQuotaDto {
    pub account: String,
    pub limit: u64,
    pub usage: u64,
    pub usage_in_drive: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TransferInfoDto {
    pub id: String,
    pub file_name: String,
    pub account: String,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub status: String,
}

impl From<TransferInfo> for TransferInfoDto {
    fn from(t: TransferInfo) -> Self {
        let status = match t.status {
            TransferStatus::Pending => "Pending".into(),
            TransferStatus::InProgress => "InProgress".into(),
            TransferStatus::Paused => "Paused".into(),
            TransferStatus::Completed => "Completed".into(),
            TransferStatus::Failed(e) => format!("Failed: {}", e),
            TransferStatus::Cancelled => "Cancelled".into(),
        };
        Self {
            id: t.id,
            file_name: t.file_name,
            account: t.account,
            progress_bytes: t.progress_bytes,
            total_bytes: t.total_bytes,
            status,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SyncStatusDto {
    pub account: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SettingsDto {
    pub mount_point: String,
    pub cache_size_bytes: u64,
    pub sync_interval_secs: u64,
    pub upload_policy: String,
    pub log_level: String,
}

// ─── Helper ──────────────────────────────────────────────────

async fn ensure_engine(state: &State<'_, AppState>) -> Result<(), String> {
    let read_guard = state.engine.read().await;
    if read_guard.is_some() {
        return Ok(());
    }
    drop(read_guard);

    let mut write_guard = state.engine.write().await;
    if write_guard.is_none() {
        let config = AppConfig::load_or_default();
        let engine = CoreEngine::new(config).map_err(|e| e.to_string())?;
        *write_guard = Some(engine);
    }
    Ok(())
}

fn get_engine_ref<'a>(guard: &'a Option<CoreEngine>) -> Result<&'a CoreEngine, String> {
    guard.as_ref().ok_or_else(|| "Engine not initialized".to_string())
}

// ─── Account Commands ────────────────────────────────────────

#[tauri::command]
async fn cmd_set_client_credentials(
    state: State<'_, AppState>,
    client_id: String,
    client_secret: String,
) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine
        .set_client_credentials(&client_id, &client_secret)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_has_client_credentials(state: State<'_, AppState>) -> Result<bool, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    Ok(engine.has_client_credentials())
}

#[tauri::command]
async fn cmd_get_client_credentials(state: State<'_, AppState>) -> Result<ClientCredentialsInfoDto, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let has = engine.has_client_credentials();
    let client_id = clouddrive_core::auth::OAuthManager::get_client_credentials()
        .map(|(id, _)| id)
        .unwrap_or_default();
    Ok(ClientCredentialsInfoDto {
        has_credentials: has,
        client_id,
    })
}

#[tauri::command]
async fn cmd_add_account(state: State<'_, AppState>, email_hint: Option<String>) -> Result<String, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine.add_account(email_hint.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_remove_account(state: State<'_, AppState>, email: String) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine.remove_account(&email).map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_reconnect_account(state: State<'_, AppState>, email: String) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine
        .reconnect_account(&email)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_list_accounts(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    Ok(engine.list_accounts())
}

// ─── File Browsing Commands ──────────────────────────────────

#[tauri::command]
async fn cmd_list_files(
    state: State<'_, AppState>,
    node_id: String,
) -> Result<Vec<VirtualNodeDto>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    Ok(engine
        .get_children(&node_id)
        .into_iter()
        .map(VirtualNodeDto::from)
        .collect())
}

#[tauri::command]
async fn cmd_search_files(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<VirtualNodeDto>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let results = engine.search(&query).map_err(|e| e.to_string())?;
    Ok(results.into_iter().map(VirtualNodeDto::from).collect())
}

#[tauri::command]
async fn cmd_list_media_files(
    state: State<'_, AppState>,
) -> Result<Vec<VirtualNodeDto>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let results = engine.list_media_files().map_err(|e| e.to_string())?;
    Ok(results.into_iter().map(VirtualNodeDto::from).collect())
}

#[tauri::command]
async fn cmd_get_file_preview(
    state: State<'_, AppState>,
    node_id: String,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let data = engine
        .read_content(&node_id, offset, length)
        .await
        .map_err(|e| e.to_string())?;
    Ok(data.to_vec())
}

// ─── File Operations ─────────────────────────────────────────

#[tauri::command]
async fn cmd_trash_file(state: State<'_, AppState>, node_id: String) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine.trash_file(&node_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_delete_permanently(
    state: State<'_, AppState>,
    node_id: String,
) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine
        .delete_permanently(&node_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_rename_file(
    state: State<'_, AppState>,
    node_id: String,
    new_name: String,
) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine
        .rename_file(&node_id, &new_name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_create_folder(
    state: State<'_, AppState>,
    parent_id: String,
    name: String,
) -> Result<VirtualNodeDto, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let node = engine
        .create_folder(&name, &parent_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(VirtualNodeDto::from(node))
}

// ─── Transfers & Quota ───────────────────────────────────────

#[tauri::command]
async fn cmd_get_transfers(state: State<'_, AppState>) -> Result<Vec<TransferInfoDto>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    Ok(engine
        .get_transfers()
        .into_iter()
        .map(TransferInfoDto::from)
        .collect())
}

#[tauri::command]
async fn cmd_cancel_transfer(state: State<'_, AppState>, id: String) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine.cancel_transfer(&id).map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_get_all_quotas(
    state: State<'_, AppState>,
) -> Result<Vec<AccountQuotaDto>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let quotas = engine
        .get_all_quotas()
        .await
        .map_err(|e| e.to_string())?;
    Ok(quotas
        .into_iter()
        .map(|(acc, q)| AccountQuotaDto {
            account: acc,
            limit: q.limit,
            usage: q.usage,
            usage_in_drive: q.usage_in_drive,
        })
        .collect())
}

// ─── Sync ────────────────────────────────────────────────────

#[tauri::command]
async fn cmd_force_sync(state: State<'_, AppState>) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let accounts = engine.list_accounts();
    let mut errors = Vec::new();
    for acc in &accounts {
        if let Err(e) = engine.force_sync(acc).await {
            tracing::warn!("Sync failed for {}: {}", acc, e);
            errors.push(format!("{}: {}", acc, e));
        }
    }
    if !errors.is_empty() {
        return Err(format!("Sync errors: {}", errors.join("; ")));
    }
    Ok(())
}

#[tauri::command]
async fn cmd_get_sync_status(state: State<'_, AppState>) -> Result<Vec<SyncStatusDto>, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let accounts = engine.list_accounts();
    let mut dtos = Vec::new();
    for acc in accounts {
        let status = engine.get_sync_status(&acc);
        let status_str = match status {
            SyncStatus::Idle => "Idle".to_string(),
            SyncStatus::Syncing => "Syncing".to_string(),
            SyncStatus::Error(e) => format!("Error: {}", e),
            SyncStatus::NeedsReauth => "NeedsReauth".to_string(),
            SyncStatus::Offline => "Offline".to_string(),
        };
        dtos.push(SyncStatusDto {
            account: acc,
            status: status_str,
        });
    }
    Ok(dtos)
}

// ─── Settings ────────────────────────────────────────────────

#[tauri::command]
async fn cmd_get_settings(state: State<'_, AppState>) -> Result<SettingsDto, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    let config = engine.get_config();
    let upload_policy = match &config.upload_policy {
        UploadPolicy::MostFreeSpace => "MostFreeSpace".to_string(),
        UploadPolicy::RoundRobin => "RoundRobin".to_string(),
        UploadPolicy::PinnedAccount(a) => format!("PinnedAccount:{}", a),
    };
    Ok(SettingsDto {
        mount_point: config.mount_point.to_string_lossy().to_string(),
        cache_size_bytes: config.cache_size_bytes,
        sync_interval_secs: config.sync_interval_secs,
        upload_policy,
        log_level: config.log_level.clone(),
    })
}

#[tauri::command]
async fn cmd_update_settings(
    state: State<'_, AppState>,
    settings: SettingsDto,
) -> Result<(), String> {
    ensure_engine(&state).await?;
    let mut guard = state.engine.write().await;
    let engine = guard.as_mut().ok_or("Engine not initialized")?;

    let upload_policy = if settings.upload_policy.starts_with("PinnedAccount:") {
        UploadPolicy::PinnedAccount(
            settings
                .upload_policy
                .trim_start_matches("PinnedAccount:")
                .to_string(),
        )
    } else if settings.upload_policy == "RoundRobin" {
        UploadPolicy::RoundRobin
    } else {
        UploadPolicy::MostFreeSpace
    };

    let config = AppConfig {
        mount_point: PathBuf::from(settings.mount_point),
        cache_size_bytes: settings.cache_size_bytes,
        sync_interval_secs: settings.sync_interval_secs,
        upload_policy,
        log_level: settings.log_level,
    };

    engine.update_config(config).map_err(|e| e.to_string())
}

// ─── Upload ──────────────────────────────────────────────────

#[tauri::command]
async fn cmd_upload_file(
    state: State<'_, AppState>,
    local_path: String,
    parent_id: String,
) -> Result<String, String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    engine
        .create_file(&local_path, &parent_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn cmd_download_to_file(
    state: State<'_, AppState>,
    node_id: String,
    dest_path: String,
) -> Result<(), String> {
    ensure_engine(&state).await?;
    let guard = state.engine.read().await;
    let engine = get_engine_ref(&guard)?;
    
    let node = engine
        .get_node(&node_id)
        .ok_or_else(|| "File not found".to_string())?;
    
    let file_size = node.size;
    let chunk_size: u64 = 5 * 1024 * 1024; // 5MB chunks
    
    let mut file = std::fs::File::create(&dest_path)
        .map_err(|e| format!("Failed to create file: {}", e))?;
    
    use std::io::Write;
    let mut offset: u64 = 0;
    while offset < file_size {
        let len = std::cmp::min(chunk_size, file_size - offset);
        let data = engine
            .read_content(&node_id, offset, len)
            .await
            .map_err(|e| format!("Download failed at offset {}: {}", offset, e))?;
        file.write_all(&data)
            .map_err(|e| format!("Failed to write to file: {}", e))?;
        offset += data.len() as u64;
    }
    
    file.flush()
        .map_err(|e| format!("Failed to flush file: {}", e))?;
    
    Ok(())
}

#[tauri::command]
async fn cmd_get_app_version() -> Result<String, String> {
    Ok(env!("CARGO_PKG_VERSION").to_string())
}

// ─── App Entry ───────────────────────────────────────────────

pub fn run() {
    let config = AppConfig::load_or_default();
    let initial_engine = CoreEngine::new(config).ok();

    let engine_arc = Arc::new(RwLock::new(initial_engine));
    let engine_setup = engine_arc.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            engine: engine_arc,
        })
        .setup(move |_app| {
            let engine_ref = engine_setup.clone();
            tauri::async_runtime::spawn(async move {
                let guard = engine_ref.read().await;
                if let Some(ref engine) = *guard {
                    let accounts = engine.list_accounts();
                    if !accounts.is_empty() {
                        if let Err(e) = engine.start_sync() {
                            tracing::warn!("Failed to start background sync: {}", e);
                        }
                    }
                    if let Err(e) = engine.resume_pending_uploads().await {
                        tracing::warn!("Failed to resume pending uploads: {}", e);
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd_set_client_credentials,
            cmd_has_client_credentials,
            cmd_get_client_credentials,
            cmd_add_account,
            cmd_remove_account,
            cmd_reconnect_account,
            cmd_list_accounts,
            cmd_list_files,
            cmd_search_files,
            cmd_list_media_files,
            cmd_get_file_preview,
            cmd_trash_file,
            cmd_delete_permanently,
            cmd_rename_file,
            cmd_create_folder,
            cmd_get_transfers,
            cmd_cancel_transfer,
            cmd_get_all_quotas,
            cmd_force_sync,
            cmd_get_sync_status,
            cmd_get_settings,
            cmd_update_settings,
            cmd_upload_file,
            cmd_download_to_file,
            cmd_get_app_version,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

