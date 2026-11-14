use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{
    model::{BtrfsUsage, Node},
    scan::TreeUpdate,
};

#[derive(Debug, Default)]
pub(super) struct LiveTree {
    entries: RwLock<HashMap<PathBuf, Arc<Entry>>>,
    root: Option<Arc<Entry>>,
    root_path: Option<PathBuf>,
}

#[derive(Debug)]
struct Entry {
    name: String,
    parent: Option<Weak<Entry>>,
    children: RwLock<Vec<Arc<Entry>>>,
    totals: [AtomicU64; 3],
    btrfs: bool,
    is_directory: AtomicBool,
    needs_sub_scan: AtomicBool,
}

impl LiveTree {
    pub(super) fn record(
        &mut self,
        path: &Path,
        name: String,
        update: TreeUpdate<'_>,
        max_depth: u64,
    ) {
        let TreeUpdate {
            size: usage,
            children,
            is_directory,
            btrfs,
        } = update;
        let values = [usage.disk, usage.uncompressed, usage.referenced];
        let root_path = self.root_path.get_or_insert_with(|| path.to_path_buf());
        let depth = path
            .strip_prefix(root_path)
            .map_or(u64::MAX, |relative| relative.components().count() as u64);
        if depth >= max_depth {
            Self::propagate(self.retained_ancestor(path), values);
            return;
        }
        let children = if depth + 1 >= max_depth {
            &[][..]
        } else {
            children
        };
        let entry = self.entry(path.to_path_buf(), name, None, btrfs);
        entry.is_directory.store(is_directory, Ordering::Relaxed);
        entry
            .needs_sub_scan
            .store(is_directory && depth + 1 >= max_depth, Ordering::Relaxed);
        if self.root.is_none() {
            self.root = Some(Arc::clone(&entry));
        }
        let created = children
            .iter()
            .map(|child| {
                let child_path = path.join(&child.0);
                self.entry(
                    child_path,
                    child.as_os_str().to_string_lossy().into_owned(),
                    Some(Arc::downgrade(&entry)),
                    btrfs,
                )
            })
            .collect::<Vec<_>>();
        *entry.children.write().unwrap_or_else(|e| e.into_inner()) = created;

        Self::propagate(Some(entry), values);
    }

    fn retained_ancestor(&self, path: &Path) -> Option<Arc<Entry>> {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        path.ancestors()
            .find_map(|ancestor| entries.get(ancestor).cloned())
    }

    fn propagate(mut current: Option<Arc<Entry>>, values: [u64; 3]) {
        while let Some(node) = current {
            for (total, value) in node.totals.iter().zip(values) {
                total.fetch_add(value, Ordering::Relaxed);
            }
            current = node.parent.as_ref().and_then(Weak::upgrade);
        }
    }

    fn entry(
        &self,
        path: PathBuf,
        name: String,
        parent: Option<Weak<Entry>>,
        btrfs: bool,
    ) -> Arc<Entry> {
        if let Some(entry) = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&path)
        {
            return Arc::clone(entry);
        }
        let entry = Arc::new(Entry {
            name,
            parent,
            children: RwLock::new(Vec::new()),
            totals: std::array::from_fn(|_| AtomicU64::new(0)),
            btrfs,
            is_directory: AtomicBool::new(false),
            needs_sub_scan: AtomicBool::new(false),
        });
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        Arc::clone(entries.entry(path).or_insert(entry))
    }

    pub(super) fn snapshot(&self) -> Option<Node> {
        self.root.as_deref().map(snapshot)
    }
}

fn snapshot(entry: &Entry) -> Node {
    let totals = entry
        .totals
        .each_ref()
        .map(|value| value.load(Ordering::Relaxed));
    let usage = BtrfsUsage {
        disk: totals[0],
        uncompressed: totals[1],
        referenced: totals[2],
    };
    Node {
        name: entry.name.clone(),
        size: usage.disk,
        children: entry
            .children
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|child| snapshot(child))
            .collect(),
        is_directory: entry.is_directory.load(Ordering::Relaxed),
        needs_sub_scan: entry.needs_sub_scan.load(Ordering::Relaxed),
        btrfs: entry.btrfs.then_some(usage),
    }
}
