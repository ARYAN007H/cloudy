use std::collections::HashMap;

/// Bidirectional mapping between FUSE inodes (u64) and CloudDrive VirtualNode IDs (String).
///
/// Inode 1 is reserved for the root directory by the FUSE protocol.
pub struct InodeMap {
    id_to_inode: HashMap<String, u64>,
    inode_to_id: HashMap<u64, String>,
    next_inode: u64,
}

impl Default for InodeMap {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeMap {
    /// Creates a new InodeMap and pre-populates inode 1 to map to "root".
    pub fn new() -> Self {
        let mut map = Self {
            id_to_inode: HashMap::new(),
            inode_to_id: HashMap::new(),
            next_inode: 2,
        };
        map.id_to_inode.insert("root".to_string(), 1);
        map.inode_to_id.insert(1, "root".to_string());
        map
    }

    /// Returns the existing inode for the given node_id, or assigns a new one.
    pub fn get_or_assign(&mut self, node_id: &str) -> u64 {
        if let Some(&inode) = self.id_to_inode.get(node_id) {
            inode
        } else {
            let inode = self.next_inode;
            self.next_inode += 1;
            self.id_to_inode.insert(node_id.to_string(), inode);
            self.inode_to_id.insert(inode, node_id.to_string());
            inode
        }
    }

    /// Retrieves the string node_id associated with an inode, if it exists.
    pub fn get_node_id(&self, inode: u64) -> Option<&str> {
        self.inode_to_id.get(&inode).map(|s| s.as_str())
    }

    /// Retrieves the inode associated with a string node_id, if it exists.
    #[allow(dead_code)]
    pub fn get_inode(&self, node_id: &str) -> Option<u64> {
        self.id_to_inode.get(node_id).copied()
    }

    /// Removes a node_id from the mapping (both directions).
    pub fn remove(&mut self, node_id: &str) {
        if let Some(inode) = self.id_to_inode.remove(node_id) {
            self.inode_to_id.remove(&inode);
        }
    }
}
