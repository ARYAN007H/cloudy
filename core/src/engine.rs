use crate::api::DriveClient;
use crate::auth::OAuthManager;
use crate::cache::MetadataCache;
use crate::config::AppConfig;
use crate::content_cache::ContentCache;
use crate::error::CloudError;
use crate::router::UploadRouter;
use crate::sync::SyncEngine;
use crate::tree::VirtualTree;
use crate::upload::UploadManager;
use crate::types::{StorageQuota, SyncStatus, TransferInfo, VirtualNode};
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

pub struct CoreEngine {
    config: AppConfig,
    auth: Arc<OAuthManager>,
    api: Arc<DriveClient>,
    cache: Arc<MetadataCache>,
    content_cache: Arc<ContentCache>,
    tree: Arc<VirtualTree>,
    sync: Arc<SyncEngine>,
    upload_manager: Arc<UploadManager>,
    router: Arc<UploadRouter>,
    accounts: RwLock<Vec<String>>,
}

impl CoreEngine {
    pub fn new(config: AppConfig) -> Result<Self, CloudError> {
        crate::logging::init_logging(&config.log_level);
        
        let auth = Arc::new(OAuthManager::new());
        let api = Arc::new(DriveClient::new(auth.clone()));
        
        let mut db_path = dirs::data_local_dir().unwrap_or_default();
        db_path.push("clouddrive");
        std::fs::create_dir_all(&db_path).unwrap_or_default();
        db_path.push("metadata.db");
        
        let cache = Arc::new(MetadataCache::new(db_path)?);
        
        let mut cc_path = dirs::cache_dir().unwrap_or_default();
        cc_path.push("clouddrive");
        let content_cache = Arc::new(ContentCache::new(cc_path, config.cache_size_bytes)?);
        
        let tree = Arc::new(VirtualTree::new());
        let sync = Arc::new(SyncEngine::new());
        let upload_manager = Arc::new(UploadManager::new());
        let router = Arc::new(UploadRouter::new(config.upload_policy.clone()));
        
        // Restore accounts from tokens file if available
        let mut initial_accounts = Vec::new();
        let tokens_path = OAuthManager::tokens_file_path();
        if let Ok(content) = std::fs::read_to_string(&tokens_path) {
            if let Ok(tokens) = serde_json::from_str::<std::collections::HashMap<String, String>>(&content) {
                for email in tokens.keys() {
                    initial_accounts.push(email.clone());
                }
            }
        }
        let accounts = RwLock::new(initial_accounts.clone());

        // Populate the virtual tree from SQLite cache so previously-synced files appear immediately
        if !initial_accounts.is_empty() {
            if let Err(e) = tree.rebuild_from_cache(&cache, &initial_accounts) {
                tracing::warn!("Failed to rebuild tree from cache on startup: {}", e);
            }
        }

        Ok(Self {
            config,
            auth,
            api,
            cache,
            content_cache,
            tree,
            sync,
            upload_manager,
            router,
            accounts,
        })
    }

    pub async fn add_account(&self, email_hint: Option<&str>) -> Result<String, CloudError> {
        let email = self.auth.start_auth_flow(email_hint).await?;
        if !self.accounts.read().contains(&email) {
            self.accounts.write().push(email.clone());
        }
        let all_accounts = self.accounts.read().clone();
        if let Err(e) = self.sync.full_sync(&email, self.api.clone(), self.cache.clone(), self.tree.clone(), &all_accounts).await {
            tracing::error!("Initial sync failed for {}: {}", email, e);
        }
        Ok(email)
    }

    pub fn remove_account(&self, email: &str) -> Result<(), CloudError> {
        self.auth.remove_account(email)?;
        self.cache.clear_account(email)?;
        self.accounts.write().retain(|e| e != email);
        self.tree.rebuild_from_cache(&self.cache, &self.accounts.read())?;
        Ok(())
    }

    pub async fn reconnect_account(&self, email: &str) -> Result<(), CloudError> {
        self.auth.start_auth_flow(Some(email)).await?;
        Ok(())
    }

    pub fn list_accounts(&self) -> Vec<String> {
        self.accounts.read().clone()
    }

    pub fn set_client_credentials(&self, id: &str, secret: &str) -> Result<(), CloudError> {
        OAuthManager::store_client_credentials(id, secret)
    }

    pub fn has_client_credentials(&self) -> bool {
        OAuthManager::has_client_credentials()
    }

    pub fn list_children(&self, parent_id: &str) -> Vec<VirtualNode> {
        self.tree.get_children(parent_id)
    }

    pub async fn read_content(&self, node_id: &str, offset: u64, length: u64) -> Result<Bytes, CloudError> {
        let node = self.tree.get_node(node_id).ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let drive_id = node.drive_file_id.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let account = node.account.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let file = self.cache.get_file(drive_id, account)?.ok_or_else(|| CloudError::NotFound(drive_id.into()))?;

        if node.is_workspace_file {
            let export_mime = crate::types::workspace_export_mime_and_ext(&file.mime_type).unwrap().0;
            return self.api.export_workspace_file(account, drive_id, export_mime).await;
        }

        if let Some(cached_data) = self.content_cache.read_range(drive_id, offset, length)? {
            return Ok(Bytes::from(cached_data));
        }

        let data = self.api.read_content_range(account, drive_id, offset, length).await?;
        
        // Caching logic: if it's a full read, we might want to cache it.
        // For simplicity, we skip caching partial reads here unless they encompass the whole file.
        // In a real implementation, we'd cache full files in background.

        Ok(data)
    }

    pub async fn create_file(&self, local_path: &str, parent_node_id: &str) -> Result<String, CloudError> {
        let parent = self.tree.get_node(parent_node_id).ok_or_else(|| CloudError::NotFound(parent_node_id.into()))?;
        let remote_parent_id = parent.drive_file_id.as_deref().unwrap_or("root");
        
        let file_name = std::path::Path::new(local_path).file_name().unwrap_or_default().to_string_lossy();
        let total_size = std::fs::metadata(local_path).map_err(|e| CloudError::Io(e.to_string()))?.len();

        let accounts = self.accounts.read().clone();
        let account = if let Some(acc) = &parent.account {
            acc.clone()
        } else {
            self.router.select_account(&accounts, total_size, &self.api).await?
        };

        self.upload_manager.enqueue(
            local_path,
            remote_parent_id,
            &file_name,
            &account,
            total_size,
            self.api.clone(),
            self.cache.clone()
        ).await
    }

    pub async fn trash_file(&self, node_id: &str) -> Result<(), CloudError> {
        let node = self.tree.get_node(node_id).ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let drive_id = node.drive_file_id.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let account = node.account.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        
        self.api.trash_file(account, drive_id).await?;
        self.cache.remove_file(drive_id, account)?;
        self.tree.remove_node(node_id)?;
        // Also remove the pool mirror node
        let pool_node_id = format!("pool_{}", drive_id);
        self.tree.remove_node(&pool_node_id)?;
        Ok(())
    }

    pub async fn delete_permanently(&self, node_id: &str) -> Result<(), CloudError> {
        let node = self.tree.get_node(node_id).ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let drive_id = node.drive_file_id.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let account = node.account.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        
        self.api.delete_permanently(account, drive_id).await?;
        self.cache.remove_file(drive_id, account)?;
        self.tree.remove_node(node_id)?;
        // Also remove the pool mirror node
        let pool_node_id = format!("pool_{}", drive_id);
        self.tree.remove_node(&pool_node_id)?;
        Ok(())
    }

    pub async fn rename_file(&self, node_id: &str, new_name: &str) -> Result<(), CloudError> {
        let mut node = self.tree.get_node(node_id).ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let drive_id = node.drive_file_id.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let account = node.account.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        
        self.api.rename_file(account, drive_id, new_name).await?;
        
        let mut file = self.cache.get_file(drive_id, account)?.ok_or_else(|| CloudError::NotFound(drive_id.into()))?;
        file.name = new_name.to_string();
        self.cache.upsert_file(&file, account)?;
        
        node.name = new_name.to_string();
        node.display_name = new_name.to_string();
        self.tree.update_node(node)?;
        Ok(())
    }

    pub async fn move_file(&self, node_id: &str, new_parent_node_id: &str, old_parent_node_id: &str) -> Result<(), CloudError> {
        let node = self.tree.get_node(node_id).ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let drive_id = node.drive_file_id.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        let account = node.account.as_deref().ok_or_else(|| CloudError::NotFound(node_id.into()))?;
        
        let new_parent = self.tree.get_node(new_parent_node_id).ok_or_else(|| CloudError::NotFound(new_parent_node_id.into()))?;
        let old_parent = self.tree.get_node(old_parent_node_id).ok_or_else(|| CloudError::NotFound(old_parent_node_id.into()))?;
        
        let new_remote_parent_id = new_parent.drive_file_id.as_deref().unwrap_or("root");
        let old_remote_parent_id = old_parent.drive_file_id.as_deref().unwrap_or("root");

        self.api.move_file(account, drive_id, new_remote_parent_id, old_remote_parent_id).await?;
        
        let mut file = self.cache.get_file(drive_id, account)?.ok_or_else(|| CloudError::NotFound(drive_id.into()))?;
        file.parents = vec![new_remote_parent_id.to_string()];
        self.cache.upsert_file(&file, account)?;
        
        self.tree.remove_node(node_id)?;
        self.tree.add_node(node, new_parent_node_id)?;
        Ok(())
    }

    pub async fn create_folder(&self, name: &str, parent_node_id: &str) -> Result<VirtualNode, CloudError> {
        let parent = self.tree.get_node(parent_node_id).ok_or_else(|| CloudError::NotFound(parent_node_id.into()))?;
        let remote_parent_id = parent.drive_file_id.as_deref().unwrap_or("root");
        
        let accounts = self.accounts.read().clone();
        if accounts.is_empty() { return Err(CloudError::Io("No accounts".into())); }
        
        let account = if let Some(acc) = &parent.account {
            acc.clone()
        } else {
            accounts[0].clone() // Or routing
        };

        let file = self.api.create_folder(&account, name, remote_parent_id).await?;
        self.cache.upsert_file(&file, &account)?;
        
        let node_id = format!("{}_{}", account, file.id);
        let node = VirtualNode {
            id: node_id.clone(),
            drive_file_id: Some(file.id.clone()),
            account: Some(account.clone()),
            name: file.name.clone(),
            display_name: file.name.clone(),
            is_dir: true,
            size: 0,
            mime_type: file.mime_type.clone(),
            modified_time: chrono::Utc::now(),
            is_read_only: false,
            children_ids: Vec::new(),
            is_workspace_file: false,
            thumbnail_link: None,
        };
        self.tree.add_node(node.clone(), parent_node_id)?;
        Ok(node)
    }

    pub fn search(&self, query: &str) -> Result<Vec<VirtualNode>, CloudError> {
        let results = self.cache.search(query)?;
        let mut nodes = Vec::new();
        for (file, account) in results {
            let node_id = format!("{}_{}", account, file.id);
            if let Some(node) = self.tree.get_node(&node_id) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    pub fn list_media_files(&self) -> Result<Vec<VirtualNode>, CloudError> {
        let results = self.cache.get_all_media_files()?;
        let mut nodes = Vec::new();
        for (file, account) in results {
            let node_id = format!("{}_{}", account, file.id);
            if let Some(node) = self.tree.get_node(&node_id) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    pub fn start_sync(&self) -> Result<(), CloudError> {
        self.sync.start(self.accounts.read().clone(), self.config.sync_interval_secs, self.api.clone(), self.cache.clone(), self.tree.clone())
    }

    pub fn stop_sync(&self) {
        self.sync.stop();
    }

    pub async fn force_sync(&self, account: &str) -> Result<(), CloudError> {
        let all_accounts = self.accounts.read().clone();
        self.sync.force_sync(account, self.api.clone(), self.cache.clone(), self.tree.clone(), &all_accounts).await
    }

    pub fn get_sync_status(&self, account: &str) -> SyncStatus {
        self.sync.get_status(account)
    }

    pub fn get_transfers(&self) -> Vec<TransferInfo> {
        self.upload_manager.get_transfers()
    }

    pub fn cancel_transfer(&self, id: &str) -> Result<(), CloudError> {
        self.upload_manager.cancel(id)
    }

    pub async fn resume_pending_uploads(&self) -> Result<Vec<String>, CloudError> {
        self.upload_manager.resume_pending(self.api.clone(), self.cache.clone()).await
    }

    pub async fn get_quota(&self, account: &str) -> Result<StorageQuota, CloudError> {
        self.api.get_quota(account).await
    }

    pub async fn get_all_quotas(&self) -> Result<HashMap<String, StorageQuota>, CloudError> {
        let accounts: Vec<String> = self.accounts.read().clone();
        let mut quotas = HashMap::new();
        for acc in &accounts {
            if let Ok(quota) = self.api.get_quota(acc).await {
                quotas.insert(acc.clone(), quota);
            }
        }
        Ok(quotas)
    }

    pub fn get_config(&self) -> AppConfig {
        self.config.clone()
    }

    pub fn update_config(&mut self, config: AppConfig) -> Result<(), CloudError> {
        config.save()?;
        self.config = config.clone();
        self.router.set_policy(config.upload_policy);
        Ok(())
    }

    pub fn get_node(&self, id: &str) -> Option<VirtualNode> {
        self.tree.get_node(id)
    }

    pub fn get_children(&self, node_id: &str) -> Vec<VirtualNode> {
        self.tree.get_children(node_id)
    }

    pub fn lookup_child(&self, parent_id: &str, name: &str) -> Option<VirtualNode> {
        self.tree.lookup_child(parent_id, name)
    }
}
