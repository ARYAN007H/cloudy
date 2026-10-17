//! Cloudy desktop app: Tauri commands on top of `clouddrive-core`.

mod media_server;

use clouddrive_core::{
    AccountSummary, AddAccountResult, AppConfig, CloudError, CoreEngine, EngineEvent, SecretBackend, TransferInfo,
    TrashItem, UploadPolicy, VirtualNode,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::broadcast::error::RecvError;

const EVENT: &str = "cloudy://event";

// ------------------------------------------------------------------ errors

#[derive(Serialize, Debug)]
pub struct CmdError {
    code: String,
    message: String,
}

impl From<CloudError> for CmdError {
    fn from(e: CloudError) -> Self {
        CmdError { code: e.code().to_string(), message: e.to_string() }
    }
}

impl CmdError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

type CmdResult<T> = Result<T, CmdError>;

// ------------------------------------------------------------------- state

struct Runtime {
    engine: Arc<CoreEngine>,
    media_base: String,
}

struct AppState {
    app: OnceLock<AppHandle>,
    cell: tokio::sync::OnceCell<Result<Arc<Runtime>, String>>,
    download_dirs: std::sync::Mutex<Vec<PathBuf>>,
}

impl AppState {
    fn new() -> Self {
        Self { app: OnceLock::new(), cell: tokio::sync::OnceCell::new(), download_dirs: Default::default() }
    }

    /// The engine, created on first use *off* the UI thread. A startup
    /// failure is reported to the UI instead of being swallowed.
    async fn rt(&self) -> CmdResult<Arc<Runtime>> {
        let app = self.app.get().cloned();
        let r = self
            .cell
            .get_or_init(|| async move {
                let app = app.ok_or_else(|| "application handle missing".to_string())?;
                build_runtime(app).await
            })
            .await;
        r.clone().map_err(|m| CmdError::new("startup", m))
    }

    async fn engine(&self) -> CmdResult<Arc<CoreEngine>> {
        Ok(self.rt().await?.engine.clone())
    }
}

async fn build_runtime(app: AppHandle) -> Result<Arc<Runtime>, String> {
    let engine = tokio::task::spawn_blocking(|| CoreEngine::new(AppConfig::load_or_default()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let engine = Arc::new(engine);
    engine.set_runtime(tokio::runtime::Handle::current());

    // Push engine events to the web view (no polling).
    let mut rx = engine.subscribe();
    let emitter = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let _ = emitter.emit(EVENT, &ev);
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = emitter.emit(EVENT, EngineEvent::TreeChanged);
                    let _ = emitter.emit(EVENT, EngineEvent::AccountsChanged);
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    let media_base = media_server::start(engine.clone()).await.map_err(|e| format!("media server: {e}"))?;

    if let Err(e) = engine.start_sync() {
        tracing::error!("could not start background sync: {e}");
    }
    let e2 = engine.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = e2.resume_pending_uploads().await {
            tracing::warn!("could not resume uploads: {e}");
        }
    });
    Ok(Arc::new(Runtime { engine, media_base }))
}

// -------------------------------------------------------------------- DTOs

#[derive(Serialize)]
pub struct NodeDto {
    id: String,
    name: String,
    is_dir: bool,
    size: u64,
    mime_type: String,
    /// Milliseconds since the epoch; 0 for synthetic folders.
    modified: i64,
    read_only: bool,
    workspace: bool,
    /// Accounts that hold this item (more than one for merged pool folders).
    accounts: Vec<String>,
    has_thumb: bool,
    child_count: usize,
    parent_id: Option<String>,
    synthetic: bool,
    /// Human-readable location (only filled for search results).
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

impl From<&VirtualNode> for NodeDto {
    fn from(n: &VirtualNode) -> Self {
        let mut accounts: Vec<String> = n.all_sources().into_iter().map(|s| s.account).collect();
        if accounts.is_empty() {
            accounts.extend(n.account.clone());
        }
        accounts.sort();
        accounts.dedup();
        NodeDto {
            id: n.id.clone(),
            name: n.display_name.clone(),
            is_dir: n.is_dir,
            size: n.size,
            mime_type: n.mime_type.clone(),
            modified: if n.drive_file_id.is_some() { n.modified_time.timestamp_millis() } else { 0 },
            read_only: n.is_read_only,
            workspace: n.is_workspace_file,
            accounts,
            has_thumb: !n.is_dir && (n.thumbnail_link.is_some() || n.mime_type.starts_with("image/")),
            child_count: n.children_ids.len(),
            parent_id: n.parent_id.clone(),
            synthetic: n.drive_file_id.is_none(),
            path: None,
        }
    }
}

fn dtos(v: Vec<VirtualNode>) -> Vec<NodeDto> {
    v.iter().map(NodeDto::from).collect()
}

#[derive(Serialize)]
pub struct SettingsDto {
    sync_interval_secs: u64,
    cache_size_mb: u64,
    cache_used_mb: u64,
    /// "most_free" | "round_robin" | "pinned"
    upload_policy: String,
    pinned_account: Option<String>,
    secure_storage: bool,
    client_id_hint: Option<String>,
    version: &'static str,
}

#[derive(Deserialize)]
pub struct SettingsUpdate {
    sync_interval_secs: Option<u64>,
    cache_size_mb: Option<u64>,
    upload_policy: Option<String>,
    pinned_account: Option<String>,
}

fn settings_dto(e: &CoreEngine) -> SettingsDto {
    let c = e.get_config();
    let (policy, pinned) = match &c.upload_policy {
        UploadPolicy::MostFreeSpace => ("most_free", None),
        UploadPolicy::RoundRobin => ("round_robin", None),
        UploadPolicy::PinnedAccount(a) => ("pinned", Some(a.clone())),
    };
    SettingsDto {
        sync_interval_secs: c.sync_interval_secs,
        cache_size_mb: c.cache_size_bytes / (1024 * 1024),
        cache_used_mb: e.cache_usage() / (1024 * 1024),
        upload_policy: policy.into(),
        pinned_account: pinned,
        secure_storage: e.storage_backend() == SecretBackend::Keyring,
        client_id_hint: e.client_id_hint(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[derive(Serialize)]
pub struct BootDto {
    version: &'static str,
    has_credentials: bool,
    media_base: String,
    accounts: Vec<AccountSummary>,
    settings: SettingsDto,
}

#[derive(Serialize)]
pub struct ViewDto {
    node: NodeDto,
    breadcrumbs: Vec<NodeDto>,
    children: Vec<NodeDto>,
}

#[derive(Serialize, Default)]
pub struct BatchResult {
    ok: usize,
    failed: Vec<String>,
}

// ---------------------------------------------------------------- commands

type St<'a> = State<'a, Arc<AppState>>;

#[tauri::command]
async fn cmd_boot(state: St<'_>) -> CmdResult<BootDto> {
    let rt = state.rt().await?;
    Ok(BootDto {
        version: env!("CARGO_PKG_VERSION"),
        has_credentials: rt.engine.has_client_credentials(),
        media_base: rt.media_base.clone(),
        accounts: rt.engine.account_summaries_local(),
        settings: settings_dto(&rt.engine),
    })
}

#[tauri::command]
async fn cmd_set_credentials(state: St<'_>, client_id: String, client_secret: String) -> CmdResult<()> {
    let e = state.engine().await?;
    tokio::task::spawn_blocking(move || e.set_client_credentials(&client_id, &client_secret))
        .await
        .map_err(|e| CmdError::new("internal", e.to_string()))??;
    Ok(())
}

#[tauri::command]
async fn cmd_import_credentials_file(state: St<'_>, path: String) -> CmdResult<()> {
    let text = std::fs::read_to_string(&path).map_err(|e| CmdError::new("io", format!("Could not read the file: {e}")))?;
    let e = state.engine().await?;
    tokio::task::spawn_blocking(move || e.set_client_credentials(&text, ""))
        .await
        .map_err(|e| CmdError::new("internal", e.to_string()))??;
    Ok(())
}

#[tauri::command]
async fn cmd_get_accounts(state: St<'_>, force: bool) -> CmdResult<Vec<AccountSummary>> {
    Ok(state.engine().await?.account_summaries(force).await)
}

#[tauri::command]
async fn cmd_add_account(app: AppHandle, state: St<'_>, hint: Option<String>) -> CmdResult<AddAccountResult> {
    let e = state.engine().await?;
    let emitter = app.clone();
    let res = e
        .add_account(hint.as_deref(), move |url| {
            let _ = emitter.emit("cloudy://auth-url", url.to_string());
        })
        .await?;
    Ok(res)
}

#[tauri::command]
async fn cmd_reconnect_account(app: AppHandle, state: St<'_>, email: String) -> CmdResult<AddAccountResult> {
    let e = state.engine().await?;
    let emitter = app.clone();
    Ok(e.reconnect_account(&email, move |url| {
        let _ = emitter.emit("cloudy://auth-url", url.to_string());
    })
    .await?)
}

#[tauri::command]
async fn cmd_cancel_add_account(state: St<'_>) -> CmdResult<()> {
    state.engine().await?.cancel_add_account();
    Ok(())
}

#[tauri::command]
async fn cmd_remove_account(state: St<'_>, email: String) -> CmdResult<()> {
    let e = state.engine().await?;
    tokio::task::spawn_blocking(move || e.remove_account(&email))
        .await
        .map_err(|e| CmdError::new("internal", e.to_string()))??;
    Ok(())
}

#[tauri::command]
async fn cmd_force_sync(state: St<'_>, account: Option<String>) -> CmdResult<()> {
    let e = state.engine().await?;
    match account {
        Some(a) => e.force_sync(&a).await?,
        None => e.sync_all_now(),
    }
    Ok(())
}

#[tauri::command]
async fn cmd_view(state: St<'_>, id: String) -> CmdResult<ViewDto> {
    let e = state.engine().await?;
    let node = e.get_node(&id).ok_or_else(|| CmdError::new("not_found", "That folder no longer exists"))?;
    let breadcrumbs = dtos(e.get_ancestors(&id));
    let children = dtos(e.get_children(&id));
    Ok(ViewDto { node: NodeDto::from(&node), breadcrumbs, children })
}

#[tauri::command]
async fn cmd_search(state: St<'_>, query: String) -> CmdResult<Vec<NodeDto>> {
    let e = state.engine().await?;
    let e2 = e.clone();
    let nodes = tokio::task::spawn_blocking(move || e2.search(&query))
        .await
        .map_err(|e| CmdError::new("internal", e.to_string()))??;
    Ok(nodes
        .iter()
        .map(|n| {
            let mut d = NodeDto::from(n);
            d.path = Some(e.node_path(&n.id));
            d
        })
        .collect())
}

#[tauri::command]
async fn cmd_list_media(state: St<'_>, offset: i64, limit: i64) -> CmdResult<Vec<NodeDto>> {
    let e = state.engine().await?;
    let nodes = tokio::task::spawn_blocking(move || e.list_media_page(offset, limit))
        .await
        .map_err(|e| CmdError::new("internal", e.to_string()))??;
    Ok(dtos(nodes))
}

#[tauri::command]
async fn cmd_node_path(state: St<'_>, id: String) -> CmdResult<String> {
    Ok(state.engine().await?.node_path(&id))
}

#[tauri::command]
async fn cmd_create_folder(state: St<'_>, name: String, parent_id: String) -> CmdResult<NodeDto> {
    let n = state.engine().await?.create_folder(&name, &parent_id).await?;
    Ok(NodeDto::from(&n))
}

#[tauri::command]
async fn cmd_rename(state: St<'_>, id: String, name: String) -> CmdResult<()> {
    Ok(state.engine().await?.rename_file(&id, &name).await?)
}

#[tauri::command]
async fn cmd_trash(state: St<'_>, ids: Vec<String>) -> CmdResult<BatchResult> {
    let e = state.engine().await?;
    let mut r = BatchResult::default();
    for id in ids {
        match e.trash_file(&id).await {
            Ok(()) => r.ok += 1,
            Err(err) => r.failed.push(err.to_string()),
        }
    }
    Ok(r)
}

#[tauri::command]
async fn cmd_move(state: St<'_>, ids: Vec<String>, new_parent_id: String) -> CmdResult<BatchResult> {
    let e = state.engine().await?;
    let mut r = BatchResult::default();
    for id in ids {
        let old = e.get_node(&id).and_then(|n| n.parent_id).unwrap_or_default();
        match e.move_file(&id, &new_parent_id, &old).await {
            Ok(()) => r.ok += 1,
            Err(err) => r.failed.push(err.to_string()),
        }
    }
    Ok(r)
}

#[tauri::command]
async fn cmd_upload(state: St<'_>, paths: Vec<String>, parent_id: String) -> CmdResult<usize> {
    let e = state.engine().await?;
    let ids = e.upload_paths(paths.into_iter().map(PathBuf::from).collect(), &parent_id).await?;
    Ok(ids.len())
}

#[tauri::command]
async fn cmd_download(state: St<'_>, ids: Vec<String>, dest_dir: String) -> CmdResult<usize> {
    let e = state.engine().await?;
    let dest = PathBuf::from(&dest_dir);
    if !dest.is_dir() {
        return Err(CmdError::new("io", "The chosen download folder does not exist"));
    }
    if let Ok(mut g) = state.download_dirs.lock() {
        g.push(dest.clone());
    }
    let mut n = 0;
    for id in ids {
        n += e.download_node(&id, dest.clone()).await?.len();
    }
    Ok(n)
}

#[tauri::command]
async fn cmd_transfers(state: St<'_>) -> CmdResult<Vec<TransferInfo>> {
    Ok(state.engine().await?.get_transfers())
}

#[tauri::command]
async fn cmd_cancel_transfer(state: St<'_>, id: String) -> CmdResult<()> {
    Ok(state.engine().await?.cancel_transfer(&id)?)
}

#[tauri::command]
async fn cmd_retry_transfer(state: St<'_>, id: String) -> CmdResult<bool> {
    Ok(state.engine().await?.retry_transfer(&id))
}

#[tauri::command]
async fn cmd_clear_transfers(state: St<'_>) -> CmdResult<()> {
    state.engine().await?.clear_finished_transfers();
    Ok(())
}

#[tauri::command]
async fn cmd_list_trash(state: St<'_>) -> CmdResult<Vec<TrashItem>> {
    Ok(state.engine().await?.list_trash().await?)
}

#[tauri::command]
async fn cmd_restore_trashed(state: St<'_>, account: String, id: String) -> CmdResult<()> {
    Ok(state.engine().await?.restore_trashed(&account, &id).await?)
}

#[tauri::command]
async fn cmd_delete_trashed(state: St<'_>, account: String, id: String) -> CmdResult<()> {
    Ok(state.engine().await?.delete_trashed(&account, &id).await?)
}

#[tauri::command]
async fn cmd_empty_trash(state: St<'_>, account: Option<String>) -> CmdResult<()> {
    Ok(state.engine().await?.empty_trash(account.as_deref()).await?)
}

#[tauri::command]
async fn cmd_get_settings(state: St<'_>) -> CmdResult<SettingsDto> {
    let e = state.engine().await?;
    Ok(settings_dto(&e))
}

#[tauri::command]
async fn cmd_update_settings(state: St<'_>, update: SettingsUpdate) -> CmdResult<SettingsDto> {
    let e = state.engine().await?;
    let mut cfg = e.get_config();
    if let Some(s) = update.sync_interval_secs {
        cfg.sync_interval_secs = s;
    }
    if let Some(mb) = update.cache_size_mb {
        cfg.cache_size_bytes = mb.saturating_mul(1024 * 1024);
    }
    if let Some(p) = update.upload_policy.as_deref() {
        cfg.upload_policy = match p {
            "most_free" => UploadPolicy::MostFreeSpace,
            "round_robin" => UploadPolicy::RoundRobin,
            "pinned" => {
                let acct = update
                    .pinned_account
                    .filter(|a| e.list_accounts().contains(a))
                    .ok_or_else(|| CmdError::new("config", "Choose which account to use"))?;
                UploadPolicy::PinnedAccount(acct)
            }
            other => return Err(CmdError::new("config", format!("Unknown upload policy \"{other}\""))),
        };
    }
    e.update_config(cfg)?;
    Ok(settings_dto(&e))
}

#[tauri::command]
async fn cmd_clear_cache(state: St<'_>) -> CmdResult<()> {
    let e = state.engine().await?;
    tokio::task::spawn_blocking(move || e.clear_cache())
        .await
        .map_err(|e| CmdError::new("internal", e.to_string()))??;
    Ok(())
}

const ALLOWED_HOSTS: [&str; 5] =
    ["console.cloud.google.com", "accounts.google.com", "myaccount.google.com", "developers.google.com", "support.google.com"];

/// Open a (Google-only) link in the system browser.
#[tauri::command]
async fn cmd_open_external(url: String) -> CmdResult<()> {
    let parsed = url::Url::parse(&url).map_err(|_| CmdError::new("config", "Invalid link"))?;
    let ok = parsed.scheme() == "https" && parsed.host_str().map_or(false, |h| ALLOWED_HOSTS.contains(&h));
    if !ok {
        return Err(CmdError::new("config", "This link is not allowed"));
    }
    open::that_detached(parsed.as_str()).map_err(|e| CmdError::new("io", e.to_string()))
}

/// Reveal a folder the user downloaded into (nothing else may be opened).
#[tauri::command]
async fn cmd_open_path(state: St<'_>, path: String) -> CmdResult<()> {
    let p = PathBuf::from(&path);
    let allowed = state.download_dirs.lock().map(|g| g.iter().any(|d| p.starts_with(d))).unwrap_or(false);
    if !allowed || !p.exists() {
        return Err(CmdError::new("io", "That location can't be opened"));
    }
    open::that_detached(p).map_err(|e| CmdError::new("io", e.to_string()))
}

// --------------------------------------------------------------------- run

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::new(AppState::new()))
        .setup(|app| {
            let state: Arc<AppState> = app.state::<Arc<AppState>>().inner().clone();
            let _ = state.app.set(app.handle().clone());
            // Warm the engine up while the window is loading.
            let s2 = state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = s2.rt().await {
                    tracing::error!("startup failed: {}", e.message);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd_boot,
            cmd_set_credentials,
            cmd_import_credentials_file,
            cmd_get_accounts,
            cmd_add_account,
            cmd_reconnect_account,
            cmd_cancel_add_account,
            cmd_remove_account,
            cmd_force_sync,
            cmd_view,
            cmd_search,
            cmd_list_media,
            cmd_node_path,
            cmd_create_folder,
            cmd_rename,
            cmd_trash,
            cmd_move,
            cmd_upload,
            cmd_download,
            cmd_transfers,
            cmd_cancel_transfer,
            cmd_retry_transfer,
            cmd_clear_transfers,
            cmd_list_trash,
            cmd_restore_trashed,
            cmd_delete_trashed,
            cmd_empty_trash,
            cmd_get_settings,
            cmd_update_settings,
            cmd_clear_cache,
            cmd_open_external,
            cmd_open_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Cloudy");
}
