use crate::cache::MetadataCache;
use crate::error::CloudError;
use crate::types::{VirtualNode, workspace_export_mime_and_ext};
use parking_lot::RwLock;
use std::collections::HashMap;
use chrono::{DateTime, Utc};

pub struct VirtualTree {
    nodes: RwLock<HashMap<String, VirtualNode>>,
}

impl VirtualTree {
    pub fn new() -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
        }
    }

    pub fn rebuild_from_cache(&self, cache: &MetadataCache, accounts: &[String]) -> Result<(), CloudError> {
        let mut new_nodes = HashMap::new();

        let root = VirtualNode {
            id: "root".to_string(),
            drive_file_id: None,
            account: None,
            name: "CloudDrive".to_string(),
            display_name: "CloudDrive".to_string(),
            is_dir: true,
            size: 0,
            mime_type: "inode/directory".to_string(),
            modified_time: Utc::now(),
            is_read_only: true,
            children_ids: vec!["pool".to_string(), "accounts".to_string()],
            is_workspace_file: false,
            thumbnail_link: None,
        };
        new_nodes.insert("root".to_string(), root);

        let mut pool_children = Vec::new();
        let mut accounts_children = Vec::new();

        let mut pooled_names: HashMap<String, usize> = HashMap::new();

        for account in accounts {
            let acc_root_id = account.clone();
            accounts_children.push(acc_root_id.clone());

            let files = cache.get_all_root_files(account)?;
            let mut acc_children = Vec::new();

            for file in &files {
                let (acc_child_id, pool_child_id) = Self::insert_file_nodes_recursive(
                    cache, account, file, &mut new_nodes, &mut pool_children, &mut pooled_names,
                )?;
                acc_children.push(acc_child_id);
                pool_children.push(pool_child_id);
            }

            let acc_root = VirtualNode {
                id: acc_root_id.clone(),
                drive_file_id: None,
                account: Some(account.clone()),
                name: account.clone(),
                display_name: account.clone(),
                is_dir: true,
                size: 0,
                mime_type: "inode/directory".to_string(),
                modified_time: Utc::now(),
                is_read_only: true,
                children_ids: acc_children,
                is_workspace_file: false,
                thumbnail_link: None,
            };
            new_nodes.insert(acc_root_id, acc_root);
        }

        let pool = VirtualNode {
            id: "pool".to_string(),
            drive_file_id: None,
            account: None,
            name: "Pool".to_string(),
            display_name: "Pool".to_string(),
            is_dir: true,
            size: 0,
            mime_type: "inode/directory".to_string(),
            modified_time: Utc::now(),
            is_read_only: false,
            children_ids: pool_children,
            is_workspace_file: false,
            thumbnail_link: None,
        };
        new_nodes.insert("pool".to_string(), pool);

        let accounts_node = VirtualNode {
            id: "accounts".to_string(),
            drive_file_id: None,
            account: None,
            name: "Accounts".to_string(),
            display_name: "Accounts".to_string(),
            is_dir: true,
            size: 0,
            mime_type: "inode/directory".to_string(),
            modified_time: Utc::now(),
            is_read_only: true,
            children_ids: accounts_children,
            is_workspace_file: false,
            thumbnail_link: None,
        };
        new_nodes.insert("accounts".to_string(), accounts_node);

        *self.nodes.write() = new_nodes;
        Ok(())
    }

    /// Recursively creates VirtualNode entries for a file/folder and all its descendants.
    /// Returns (account_node_id, pool_node_id) for the top-level item.
    fn insert_file_nodes_recursive(
        cache: &MetadataCache,
        account: &str,
        file: &crate::types::DriveFile,
        nodes: &mut HashMap<String, VirtualNode>,
        pool_children: &mut Vec<String>,
        pooled_names: &mut HashMap<String, usize>,
    ) -> Result<(String, String), CloudError> {
        let node_id = format!("{}_{}", account, file.id);
        let pool_node_id = format!("pool_{}", file.id);

        let mut display_name = file.name.clone();
        let mut is_workspace_file = false;
        let mut is_read_only = false;

        if let Some((_, ext)) = workspace_export_mime_and_ext(&file.mime_type) {
            display_name.push_str(ext);
            is_workspace_file = true;
            is_read_only = true;
        }

        // Recursively populate children for folders
        let mut acc_child_ids = Vec::new();
        let mut pool_child_ids = Vec::new();

        if file.is_folder {
            if let Ok(children) = cache.get_children(&file.id, account) {
                for child_file in &children {
                    let (acc_cid, pool_cid) = Self::insert_file_nodes_recursive(
                        cache, account, child_file, nodes, pool_children, pooled_names,
                    )?;
                    acc_child_ids.push(acc_cid);
                    pool_child_ids.push(pool_cid);
                }
            }
        }

        let modified = DateTime::parse_from_rfc3339(&file.modified_time)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        // Account-specific node
        let node = VirtualNode {
            id: node_id.clone(),
            drive_file_id: Some(file.id.clone()),
            account: Some(account.to_string()),
            name: file.name.clone(),
            display_name: display_name.clone(),
            is_dir: file.is_folder,
            size: file.size.unwrap_or(0),
            mime_type: file.mime_type.clone(),
            modified_time: modified,
            is_read_only,
            children_ids: acc_child_ids,
            is_workspace_file,
            thumbnail_link: file.thumbnail_link.clone(),
        };
        nodes.insert(node_id.clone(), node);

        // Pool mirror node (only add root-level pool items to pool_children;
        // sub-items are children of their pool parent)
        let count = pooled_names.entry(display_name.clone()).or_insert(0);
        *count += 1;
        let final_display_name = if *count > 1 {
            let suffix = file.id.chars().take(6).collect::<String>();
            format!("{} ({})", display_name, suffix)
        } else {
            display_name
        };

        let pool_node = VirtualNode {
            id: pool_node_id.clone(),
            drive_file_id: Some(file.id.clone()),
            account: Some(account.to_string()),
            name: file.name.clone(),
            display_name: final_display_name,
            is_dir: file.is_folder,
            size: file.size.unwrap_or(0),
            mime_type: file.mime_type.clone(),
            modified_time: modified,
            is_read_only,
            children_ids: pool_child_ids,
            is_workspace_file,
            thumbnail_link: file.thumbnail_link.clone(),
        };
        nodes.insert(pool_node_id.clone(), pool_node);

        // Only top-level files are added to pool_children by the caller;
        // recursive children are wired via their parent's children_ids.
        // We DON'T push to pool_children here — the caller handles root-level items.

        Ok((node_id, pool_node_id))
    }

    pub fn get_node(&self, id: &str) -> Option<VirtualNode> {
        self.nodes.read().get(id).cloned()
    }

    pub fn get_children(&self, node_id: &str) -> Vec<VirtualNode> {
        let mut res = Vec::new();
        if let Some(node) = self.nodes.read().get(node_id) {
            for child_id in &node.children_ids {
                if let Some(child) = self.nodes.read().get(child_id) {
                    res.push(child.clone());
                }
            }
        }
        res
    }

    pub fn lookup_child(&self, parent_id: &str, name: &str) -> Option<VirtualNode> {
        if let Some(parent) = self.nodes.read().get(parent_id) {
            for child_id in &parent.children_ids {
                if let Some(child) = self.nodes.read().get(child_id) {
                    if child.display_name == name {
                        return Some(child.clone());
                    }
                }
            }
        }
        None
    }

    pub fn get_node_path(&self, _node_id: &str) -> String {
        // Since nodes only have children_ids and not parent_ids in this struct, 
        // a full path lookup would require traversing from root.
        // This is a simplified implementation.
        "/".to_string()
    }

    pub fn find_nodes_by_drive_id(&self, drive_id: &str, account: &str) -> Vec<String> {
        let mut ids = Vec::new();
        for (id, node) in self.nodes.read().iter() {
            if node.drive_file_id.as_deref() == Some(drive_id) && node.account.as_deref() == Some(account) {
                ids.push(id.clone());
            }
        }
        ids
    }

    pub fn add_node(&self, node: VirtualNode, parent_id: &str) -> Result<(), CloudError> {
        let mut nodes = self.nodes.write();
        let node_id = node.id.clone();
        if let Some(parent) = nodes.get_mut(parent_id) {
            parent.children_ids.push(node_id.clone());
        }
        nodes.insert(node_id, node);
        Ok(())
    }

    pub fn remove_node(&self, node_id: &str) -> Result<(), CloudError> {
        let mut nodes = self.nodes.write();
        // Remove from all parents
        for node in nodes.values_mut() {
            node.children_ids.retain(|id| id != node_id);
        }
        nodes.remove(node_id);
        Ok(())
    }

    pub fn update_node(&self, node: VirtualNode) -> Result<(), CloudError> {
        let mut nodes = self.nodes.write();
        nodes.insert(node.id.clone(), node);
        Ok(())
    }
}
