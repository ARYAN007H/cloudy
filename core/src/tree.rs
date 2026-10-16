//! In-memory virtual file tree built from the metadata cache.
//!
//! ```text
//! root
//! ├── pool        all accounts merged: same-named folders are unified
//! └── accounts
//!     └── <email> one subtree per account (exactly what Drive shows)
//! ```

use crate::cache::MetadataCache;
use crate::error::CloudError;
use crate::types::{workspace_export_mime_and_ext, DriveFile, NodeSource, VirtualNode};
use chrono::{DateTime, TimeZone, Utc};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};

const MAX_DEPTH: usize = 256;

pub struct VirtualTree {
    nodes: RwLock<HashMap<String, VirtualNode>>,
}

impl Default for VirtualTree {
    fn default() -> Self {
        Self::new()
    }
}

struct AcctIndex<'a> {
    by_parent: HashMap<&'a str, Vec<&'a DriveFile>>,
    roots: Vec<&'a DriveFile>,
}

fn index_account(files: &[DriveFile]) -> AcctIndex<'_> {
    let ids: HashSet<&str> = files.iter().map(|f| f.id.as_str()).collect();
    let mut by_parent: HashMap<&str, Vec<&DriveFile>> = HashMap::new();
    let mut roots = Vec::new();
    for f in files {
        let parent = f.parents.first().map(String::as_str).unwrap_or("root");
        if parent != f.id && ids.contains(parent) {
            by_parent.entry(parent).or_default().push(f);
        } else {
            roots.push(f);
        }
    }
    AcctIndex { by_parent, roots }
}

fn epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).single().unwrap()
}

fn parse_time(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)).unwrap_or_else(|_| epoch())
}

fn short_id(id: &str) -> String {
    let n = id.chars().count();
    id.chars().skip(n.saturating_sub(6)).collect()
}

/// `photo.jpg` + `a1b2c3` → `photo (a1b2c3).jpg`
fn suffixed(name: &str, id: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 8 => format!("{} ({}){}", &name[..i], short_id(id), &name[i..]),
        _ => format!("{} ({})", name, short_id(id)),
    }
}

fn file_node(file: &DriveFile, account: &str, id: String, parent: &str, children: Vec<String>) -> VirtualNode {
    let mut display_name = file.name.clone();
    let mut is_workspace_file = false;
    if let Some((_, ext)) = workspace_export_mime_and_ext(&file.mime_type) {
        if !display_name.to_lowercase().ends_with(ext) {
            display_name.push_str(ext);
        }
        is_workspace_file = true;
    }
    VirtualNode {
        id,
        drive_file_id: Some(file.id.clone()),
        account: Some(account.to_string()),
        name: file.name.clone(),
        display_name,
        is_dir: file.is_folder,
        size: file.size.unwrap_or(0),
        mime_type: file.mime_type.clone(),
        modified_time: parse_time(&file.modified_time),
        is_read_only: is_workspace_file,
        children_ids: children,
        is_workspace_file,
        thumbnail_link: file.thumbnail_link.clone(),
        parent_id: Some(parent.to_string()),
        sources: vec![],
    }
}

fn synthetic_dir(id: &str, name: &str, read_only: bool, parent: Option<&str>, children: Vec<String>) -> VirtualNode {
    VirtualNode {
        id: id.to_string(),
        drive_file_id: None,
        account: None,
        name: name.to_string(),
        display_name: name.to_string(),
        is_dir: true,
        size: 0,
        mime_type: "inode/directory".to_string(),
        modified_time: epoch(),
        is_read_only: read_only,
        children_ids: children,
        is_workspace_file: false,
        thumbnail_link: None,
        parent_id: parent.map(str::to_string),
        sources: vec![],
    }
}

/// Order (folders first, then by name) and make names unique within one folder.
fn finalize_level(items: &mut Vec<VirtualNode>) {
    items.sort_by(|a, b| {
        (!a.is_dir, a.display_name.to_lowercase(), &a.display_name, &a.id)
            .cmp(&(!b.is_dir, b.display_name.to_lowercase(), &b.display_name, &b.id))
    });
    let mut seen: HashMap<String, usize> = HashMap::new();
    for n in items.iter_mut() {
        let c = seen.entry(n.display_name.clone()).or_insert(0);
        *c += 1;
        if *c > 1 {
            let key = n.drive_file_id.clone().unwrap_or_else(|| n.id.clone());
            n.display_name = suffixed(&n.display_name, &key);
        }
    }
}

fn latest(nodes: &HashMap<String, VirtualNode>, ids: &[String]) -> DateTime<Utc> {
    ids.iter().filter_map(|i| nodes.get(i)).map(|n| n.modified_time).max().unwrap_or_else(epoch)
}

impl VirtualTree {
    pub fn new() -> Self {
        Self { nodes: RwLock::new(HashMap::new()) }
    }

    /// Rebuild the whole tree from the cache. The new tree is assembled
    /// off-lock and swapped in atomically, so readers never see a half-built tree.
    pub fn rebuild_from_cache(&self, cache: &MetadataCache, accounts: &[String]) -> Result<(), CloudError> {
        let mut data: Vec<(String, Vec<DriveFile>)> = Vec::new();
        for a in accounts {
            data.push((a.clone(), cache.all_files_for_account(a)?));
        }
        let new_nodes = Self::build(&data);
        *self.nodes.write() = new_nodes;
        Ok(())
    }

    pub(crate) fn build(data: &[(String, Vec<DriveFile>)]) -> HashMap<String, VirtualNode> {
        let mut nodes: HashMap<String, VirtualNode> = HashMap::new();
        let indexes: Vec<AcctIndex<'_>> = data.iter().map(|(_, f)| index_account(f)).collect();
        let by_name: HashMap<&str, &AcctIndex<'_>> =
            data.iter().map(|(a, _)| a.as_str()).zip(indexes.iter()).collect();

        // ---- per-account view -------------------------------------------------
        let mut account_ids = Vec::new();
        for ((account, _), idx) in data.iter().zip(indexes.iter()) {
            account_ids.push(account.clone());
            let kids = Self::build_account_level(account, idx, &idx.roots, account, &mut nodes, 0);
            let mut dir = synthetic_dir(account, account, true, Some("accounts"), kids);
            dir.account = Some(account.clone());
            dir.modified_time = latest(&nodes, &dir.children_ids);
            nodes.insert(account.clone(), dir);
        }

        // ---- pooled view ------------------------------------------------------
        let entries: Vec<(&str, &DriveFile)> = data
            .iter()
            .zip(indexes.iter())
            .flat_map(|((a, _), idx)| idx.roots.iter().map(move |f| (a.as_str(), *f)))
            .collect();
        let pool_kids = Self::build_pool_level(&by_name, entries, "pool", &mut nodes, 0);
        let mut pool = synthetic_dir("pool", "Pool", false, Some("root"), pool_kids);
        pool.modified_time = latest(&nodes, &pool.children_ids);
        nodes.insert("pool".into(), pool);

        let acc = synthetic_dir("accounts", "Accounts", true, Some("root"), account_ids);
        nodes.insert("accounts".into(), acc);
        nodes.insert(
            "root".into(),
            synthetic_dir("root", "Cloudy", true, None, vec!["pool".into(), "accounts".into()]),
        );
        nodes
    }

    fn build_account_level(
        account: &str,
        idx: &AcctIndex<'_>,
        entries: &[&DriveFile],
        parent_id: &str,
        nodes: &mut HashMap<String, VirtualNode>,
        depth: usize,
    ) -> Vec<String> {
        let mut level: Vec<VirtualNode> = Vec::with_capacity(entries.len());
        for f in entries {
            let id = format!("{account}_{}", f.id);
            let kids = if f.is_folder && depth < MAX_DEPTH {
                match idx.by_parent.get(f.id.as_str()) {
                    Some(children) => Self::build_account_level(account, idx, children, &id, nodes, depth + 1),
                    None => vec![],
                }
            } else {
                vec![]
            };
            level.push(file_node(f, account, id, parent_id, kids));
        }
        finalize_level(&mut level);
        let ids: Vec<String> = level.iter().map(|n| n.id.clone()).collect();
        for mut n in level {
            if n.is_dir {
                n.modified_time = n.modified_time.max(latest(nodes, &n.children_ids));
            }
            nodes.insert(n.id.clone(), n);
        }
        ids
    }

    /// Build one level of the pool. Folders with the same name from different
    /// accounts become ONE folder whose children are the union of theirs.
    fn build_pool_level(
        by_name: &HashMap<&str, &AcctIndex<'_>>,
        entries: Vec<(&str, &DriveFile)>,
        parent_id: &str,
        nodes: &mut HashMap<String, VirtualNode>,
        depth: usize,
    ) -> Vec<String> {
        let mut folders: HashMap<&str, Vec<(&str, &DriveFile)>> = HashMap::new();
        let mut files: Vec<(&str, &DriveFile)> = Vec::new();
        for (a, f) in entries {
            if f.is_folder {
                folders.entry(f.name.as_str()).or_default().push((a, f));
            } else {
                files.push((a, f));
            }
        }

        let mut level: Vec<VirtualNode> = Vec::new();

        for (_, mut group) in folders {
            group.sort_by(|x, y| (x.0, &x.1.id).cmp(&(y.0, &y.1.id)));
            let (first_acct, first) = group[0];
            let id = format!("pool_{}", first.id);

            let mut child_entries: Vec<(&str, &DriveFile)> = Vec::new();
            if depth < MAX_DEPTH {
                for (a, f) in &group {
                    if let Some(idx) = by_name.get(a) {
                        if let Some(kids) = idx.by_parent.get(f.id.as_str()) {
                            child_entries.extend(kids.iter().map(|k| (*a, *k)));
                        }
                    }
                }
            }
            let kids = Self::build_pool_level(by_name, child_entries, &id, nodes, depth + 1);

            let mut node = file_node(first, first_acct, id, parent_id, kids);
            node.modified_time = group
                .iter()
                .map(|(_, f)| parse_time(&f.modified_time))
                .max()
                .unwrap_or_else(epoch)
                .max(latest(nodes, &node.children_ids));
            if group.len() > 1 {
                node.sources = group
                    .iter()
                    .map(|(a, f)| NodeSource { account: a.to_string(), drive_file_id: f.id.clone() })
                    .collect();
            }
            level.push(node);
        }

        for (a, f) in files {
            level.push(file_node(f, a, format!("pool_{}", f.id), parent_id, vec![]));
        }

        finalize_level(&mut level);
        let ids: Vec<String> = level.iter().map(|n| n.id.clone()).collect();
        for n in level {
            nodes.insert(n.id.clone(), n);
        }
        ids
    }

    // ----------------------------------------------------------------- reads
    // Every method takes the lock exactly once: re-acquiring a read lock while
    // already holding one can deadlock when a writer is queued in between.

    pub fn get_node(&self, id: &str) -> Option<VirtualNode> {
        self.nodes.read().get(id).cloned()
    }

    pub fn get_children(&self, node_id: &str) -> Vec<VirtualNode> {
        let nodes = self.nodes.read();
        match nodes.get(node_id) {
            Some(n) => n.children_ids.iter().filter_map(|c| nodes.get(c)).cloned().collect(),
            None => vec![],
        }
    }

    pub fn lookup_child(&self, parent_id: &str, name: &str) -> Option<VirtualNode> {
        let nodes = self.nodes.read();
        let parent = nodes.get(parent_id)?;
        parent.children_ids.iter().filter_map(|c| nodes.get(c)).find(|c| c.display_name == name).cloned()
    }

    /// Chain from the root down to `node_id` (inclusive).
    pub fn get_ancestors(&self, node_id: &str) -> Vec<VirtualNode> {
        let nodes = self.nodes.read();
        let mut chain = Vec::new();
        let mut cur = nodes.get(node_id);
        let mut guard = 0;
        while let Some(n) = cur {
            chain.push(n.clone());
            guard += 1;
            if guard > MAX_DEPTH + 4 {
                break;
            }
            cur = n.parent_id.as_deref().and_then(|p| nodes.get(p));
        }
        chain.reverse();
        chain
    }

    pub fn get_node_path(&self, node_id: &str) -> String {
        let chain = self.get_ancestors(node_id);
        if chain.len() <= 1 {
            return "/".to_string();
        }
        let parts: Vec<&str> = chain.iter().skip(1).map(|n| n.display_name.as_str()).collect();
        format!("/{}", parts.join("/"))
    }

    pub fn find_nodes_by_drive_id(&self, drive_id: &str, account: &str) -> Vec<String> {
        self.nodes
            .read()
            .iter()
            .filter(|(_, n)| n.drive_file_id.as_deref() == Some(drive_id) && n.account.as_deref() == Some(account))
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.read().len()
    }

    // -------------------------------------------------------------- mutation
    // Kept for callers that want an instant, optimistic update; the engine
    // normally just rebuilds from the cache after a change.

    pub fn add_node(&self, mut node: VirtualNode, parent_id: &str) -> Result<(), CloudError> {
        let mut nodes = self.nodes.write();
        node.parent_id = Some(parent_id.to_string());
        if let Some(parent) = nodes.get_mut(parent_id) {
            if !parent.children_ids.contains(&node.id) {
                parent.children_ids.push(node.id.clone());
            }
        }
        nodes.insert(node.id.clone(), node);
        Ok(())
    }

    pub fn remove_node(&self, node_id: &str) -> Result<(), CloudError> {
        let mut nodes = self.nodes.write();
        if let Some(removed) = nodes.remove(node_id) {
            if let Some(p) = removed.parent_id.as_deref().and_then(|p| nodes.get_mut(p)) {
                p.children_ids.retain(|c| c != node_id);
            }
        }
        Ok(())
    }

    pub fn update_node(&self, node: VirtualNode) -> Result<(), CloudError> {
        self.nodes.write().insert(node.id.clone(), node);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FOLDER_MIME;

    fn f(id: &str, name: &str, parent: &str, folder: bool) -> DriveFile {
        DriveFile {
            id: id.into(),
            name: name.into(),
            mime_type: if folder { FOLDER_MIME.into() } else { "image/jpeg".into() },
            parents: vec![parent.into()],
            size: Some(1),
            modified_time: "2024-01-01T00:00:00Z".into(),
            md5_checksum: None,
            trashed: false,
            is_folder: folder,
            thumbnail_link: None,
            owned_by_me: true,
        }
    }

    fn two_accounts() -> Vec<(String, Vec<DriveFile>)> {
        vec![
            (
                "a@x.com".into(),
                vec![f("A_photos", "Photos", "ROOT", true), f("A1", "IMG_1.jpg", "A_photos", false), f("A_top", "IMG_1.jpg", "ROOT", false)],
            ),
            (
                "b@x.com".into(),
                vec![f("B_photos", "Photos", "ROOT", true), f("B1", "IMG_2.jpg", "B_photos", false), f("B2", "IMG_1.jpg", "B_photos", false)],
            ),
        ]
    }

    fn tree() -> VirtualTree {
        let t = VirtualTree::new();
        *t.nodes.write() = VirtualTree::build(&two_accounts());
        t
    }

    #[test]
    fn same_named_folders_merge_in_pool() {
        let t = tree();
        let kids = t.get_children("pool");
        let photos: Vec<_> = kids.iter().filter(|n| n.display_name == "Photos").collect();
        assert_eq!(photos.len(), 1, "one merged Photos folder");
        assert_eq!(photos[0].sources.len(), 2);
        let inside: Vec<String> = t.get_children(&photos[0].id).into_iter().map(|n| n.display_name).collect();
        assert_eq!(inside.len(), 3);
        assert!(inside.contains(&"IMG_2.jpg".to_string()));
    }

    #[test]
    fn duplicate_names_are_disambiguated_per_folder_only() {
        let t = tree();
        // Inside merged Photos: IMG_1.jpg exists in both accounts → one gets a suffix.
        let photos = t.lookup_child("pool", "Photos").unwrap();
        let names: Vec<String> = t.get_children(&photos.id).into_iter().map(|n| n.display_name).collect();
        let uniq: HashSet<_> = names.iter().collect();
        assert_eq!(uniq.len(), names.len(), "names unique in folder: {names:?}");
        assert!(names.iter().any(|n| n == "IMG_1.jpg"));
        assert!(names.iter().any(|n| n.starts_with("IMG_1 (") && n.ends_with(".jpg")));
        // Top level has its own IMG_1.jpg, which must NOT have been mangled.
        assert!(t.lookup_child("pool", "IMG_1.jpg").is_some());
    }

    #[test]
    fn account_view_is_untouched_and_paths_work() {
        let t = tree();
        let a = t.get_children("a@x.com");
        assert_eq!(a.len(), 2);
        let n = t.lookup_child("a@x.com_A_photos", "IMG_1.jpg").unwrap();
        assert_eq!(n.parent_id.as_deref(), Some("a@x.com_A_photos"));
        assert_eq!(t.get_node_path(&n.id), "/Accounts/a@x.com/Photos/IMG_1.jpg");
        let chain: Vec<String> = t.get_ancestors(&n.id).into_iter().map(|x| x.id).collect();
        assert_eq!(chain.first().map(String::as_str), Some("root"));
    }

    #[test]
    fn workspace_files_get_an_extension() {
        let mut d = f("d", "Report", "ROOT", false);
        d.mime_type = "application/vnd.google-apps.document".into();
        let t = VirtualTree::new();
        *t.nodes.write() = VirtualTree::build(&[("a@x.com".into(), vec![d])]);
        let n = t.get_children("pool").pop().unwrap();
        assert_eq!(n.display_name, "Report.docx");
        assert!(n.is_workspace_file && n.is_read_only);
    }

    #[test]
    fn broken_parent_chains_cannot_loop_or_hang() {
        // x -> y -> x (cycle) and a self-parent; neither may crash the build.
        let data = vec![(
            "a@x.com".to_string(),
            vec![f("x", "x", "y", true), f("y", "y", "x", true), f("s", "s", "s", true), f("ok", "ok", "ROOT", false)],
        )];
        let t = VirtualTree::new();
        *t.nodes.write() = VirtualTree::build(&data);
        assert!(t.lookup_child("pool", "ok").is_some());
    }

    #[test]
    fn no_deadlock_under_concurrent_rebuild_and_reads() {
        use std::sync::Arc;
        let t = Arc::new(tree());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut hs = vec![];
        for _ in 0..4 {
            let (t, stop) = (t.clone(), stop.clone());
            hs.push(std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = t.get_children("pool");
                    let _ = t.lookup_child("pool", "Photos");
                }
            }));
        }
        let w = {
            let t = t.clone();
            std::thread::spawn(move || {
                for _ in 0..300 {
                    *t.nodes.write() = VirtualTree::build(&two_accounts());
                }
            })
        };
        w.join().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for h in hs {
            h.join().unwrap();
        }
    }
}
