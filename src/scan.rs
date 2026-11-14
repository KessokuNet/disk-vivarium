//! Cancellation cannot stop a pdu scan partway through; a UI can only discard its result.

mod live;

use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use parallel_disk_usage::{
    data_tree::DataTree,
    device::DeviceNumber,
    get_size::{GetApparentSize, GetBlockSize, GetSize},
    hardlink::{DeduplicateSharedSize, HardlinkAware, RecordHardlinks, RecordHardlinksArgument},
    os_string_display::OsStringDisplay,
    reporter::{ErrorReport, Event, Reporter, error_report::Operation},
    size::Bytes,
    tree_builder::{Info, TreeBuilder},
};

use crate::model::{BtrfsReport, BtrfsUsage, Node};

const MAX_RECENT_ERRORS: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SizeMode {
    #[default]
    Allocated,
    Apparent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanOptions {
    pub root: PathBuf,
    /// Prune nodes without pruning their contribution to ancestor totals.
    pub max_depth: u64,
    pub size_mode: SizeMode,
    pub cross_device: bool,
    pub dedupe_hardlinks: bool,
    /// Exact Btrfs accounting needs privilege and stays on one filesystem; otherwise use block counts.
    pub btrfs: bool,
    pub min_ratio: f32,
    pub sort: bool,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_depth: 32,
            size_mode: SizeMode::Allocated,
            cross_device: false,
            dedupe_hardlinks: true,
            btrfs: true,
            min_ratio: 0.0,
            sort: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Progress {
    entries: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    recent_errors: Mutex<Vec<String>>,
    tree: Mutex<live::LiveTree>,
}

pub(crate) struct TreeUpdate<'a> {
    pub size: BtrfsUsage,
    pub children: &'a [OsStringDisplay],
    pub is_directory: bool,
    pub btrfs: bool,
}

impl Progress {
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            entries: self.entries.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    /// Returning early while the scanner owns the tree avoids stalling the walk for a repaint.
    pub fn tree_snapshot(&self) -> Option<Node> {
        let tree = match self.tree.try_lock() {
            Ok(tree) => tree,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        tree.snapshot()
    }

    pub(crate) fn record_tree(
        &self,
        path: &std::path::Path,
        name: String,
        update: TreeUpdate<'_>,
        max_depth: u64,
    ) {
        self.tree
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(path, name, update, max_depth);
    }

    pub fn recent_errors(&self) -> Vec<String> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.recent_errors.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn record_error(&self, message: String) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut recent = self.lock();
        if recent.len() < MAX_RECENT_ERRORS {
            recent.push(message);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub entries: u64,
    pub bytes: u64,
    pub errors: u64,
}

#[derive(Debug)]
pub struct ScanOutcome {
    pub tree: Node,
    pub progress: ProgressSnapshot,
    pub errors: Vec<String>,
    pub btrfs: Option<BtrfsReport>,
    pub btrfs_unavailable: Option<String>,
}

pub fn scan(options: &ScanOptions) -> io::Result<ScanOutcome> {
    scan_reported(options, &Progress::default())
}

fn scan_reported(options: &ScanOptions, progress: &Progress) -> io::Result<ScanOutcome> {
    // Fail before the walk starts, rather than returning an empty tree for a typo'd path.
    std::fs::symlink_metadata(&options.root)?;

    let reporter = Recorder { progress };

    #[cfg(target_os = "linux")]
    let mut btrfs_unavailable = (options.btrfs && options.cross_device)
        .then(|| "exact Btrfs accounting currently requires 'Stay on one filesystem'".to_owned());
    #[cfg(target_os = "linux")]
    if options.btrfs
        && !options.cross_device
        && options.size_mode == SizeMode::Allocated
        && crate::btrfs::is_btrfs(&options.root)?
    {
        match crate::btrfs::scan(options, progress, &reporter)? {
            crate::btrfs::BtrfsScan::Complete(outcome) => return Ok(*outcome),
            crate::btrfs::BtrfsScan::Unavailable(reason) => btrfs_unavailable = Some(reason),
        }
    }

    let recorder = HardlinkAware::<Bytes>::new();

    let mut data_tree = match options.size_mode {
        SizeMode::Allocated => build(options, GetBlockSize, &recorder, &reporter),
        SizeMode::Apparent => build(options, GetApparentSize, &recorder, &reporter),
    };

    if options.dedupe_hardlinks {
        // Sizes stay inflated if this fails, which is cosmetic next to losing the whole scan.
        let _: Result<_, _> = recorder.deduplicate(&mut data_tree);
    }

    Ok(finish_tree(
        data_tree,
        options,
        progress,
        None,
        btrfs_unavailable,
    ))
}

fn build<Getter: GetSize<Size = Bytes> + Sync>(
    options: &ScanOptions,
    size_getter: Getter,
    recorder: &HardlinkAware<Bytes>,
    reporter: &Recorder,
) -> DataTree<OsStringDisplay, Bytes> {
    let root_device = std::fs::symlink_metadata(&options.root)
        .ok()
        .map(|metadata| DeviceNumber::get(&metadata));
    let root_name = OsStringDisplay::os_string_from(&options.root);
    TreeBuilder {
        name: root_name.clone(),
        path: options.root.clone(),
        get_info: |path| {
            let stats = match std::fs::symlink_metadata(path) {
                Ok(stats) => stats,
                Err(error) => {
                    report_fs_error(reporter, Operation::SymlinkMetadata, path, error);
                    return Info::default();
                }
            };
            let size = size_getter.get_size(&stats);
            let stay = options.cross_device || root_device == Some(DeviceNumber::get(&stats));
            reporter.report(Event::ReceiveData(size));
            let _: Result<(), _> = recorder.record_hardlinks(RecordHardlinksArgument {
                path,
                stats: &stats,
                size,
                reporter,
            });
            let children = if stats.is_dir() && stay {
                match std::fs::read_dir(path) {
                    Ok(entries) => entries
                        .filter_map(|entry| match entry {
                            Ok(entry) => Some(OsStringDisplay::from(entry.file_name())),
                            Err(error) => {
                                report_fs_error(reporter, Operation::AccessEntry, path, error);
                                None
                            }
                        })
                        .collect(),
                    Err(error) => {
                        report_fs_error(reporter, Operation::ReadDirectory, path, error);
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let name = if path.as_path() == options.root {
                root_name.to_string()
            } else {
                path.file_name()
                    .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
                    .into_owned()
            };
            reporter.progress.record_tree(
                path,
                name,
                TreeUpdate {
                    size: BtrfsUsage::from(size.inner()),
                    children: &children,
                    is_directory: stats.is_dir(),
                    btrfs: false,
                },
                options.max_depth,
            );
            Info { size, children }
        },
        join_path: |prefix, name| prefix.join(&name.0),
        max_depth: options.max_depth,
    }
    .into()
}

fn report_fs_error(
    reporter: &Recorder,
    operation: Operation,
    path: &std::path::Path,
    error: io::Error,
) {
    let event: Event<Bytes> = Event::EncounterError(ErrorReport {
        operation,
        path,
        error,
    });
    reporter.report(event);
}

pub(crate) trait NodeSize: parallel_disk_usage::size::Size + Send + Sync {
    fn into_node_size(self) -> (u64, Option<BtrfsUsage>);
}

impl NodeSize for Bytes {
    fn into_node_size(self) -> (u64, Option<BtrfsUsage>) {
        (self.inner(), None)
    }
}

impl NodeSize for BtrfsUsage {
    fn into_node_size(self) -> (u64, Option<BtrfsUsage>) {
        (self.disk, Some(self))
    }
}

pub(crate) fn finish_tree<S: NodeSize>(
    mut data_tree: DataTree<OsStringDisplay, S>,
    options: &ScanOptions,
    progress: &Progress,
    btrfs: Option<BtrfsReport>,
    btrfs_unavailable: Option<String>,
) -> ScanOutcome {
    if options.sort {
        data_tree.par_sort_by(|left, right| {
            right
                .size()
                .into_node_size()
                .0
                .cmp(&left.size().into_node_size().0)
        });
    }

    if options.min_ratio > 0.0 {
        let minimal = options.min_ratio as f64 * data_tree.size().into_node_size().0 as f64;
        data_tree.par_retain(|node, _| node.size().into_node_size().0 as f64 >= minimal);
    }

    ScanOutcome {
        tree: convert(&data_tree, &options.root, options.max_depth, 0),
        progress: progress.snapshot(),
        errors: progress.recent_errors(),
        btrfs,
        btrfs_unavailable,
    }
}

fn convert<S: NodeSize>(
    data_tree: &DataTree<OsStringDisplay, S>,
    path: &std::path::Path,
    max_depth: u64,
    depth: u64,
) -> Node {
    let (size, btrfs) = data_tree.size().into_node_size();
    // pdu exposes no file-kind getter; only retained nodes need this second metadata read.
    let is_directory = std::fs::symlink_metadata(path).is_ok_and(|stats| stats.is_dir())
        || !data_tree.children().is_empty();
    let children = data_tree
        .children()
        .iter()
        .map(|child| convert(child, &path.join(&child.name().0), max_depth, depth + 1))
        .collect();
    Node {
        name: data_tree.name().as_os_str().to_string_lossy().into_owned(),
        size,
        children,
        is_directory,
        needs_sub_scan: is_directory && depth + 1 >= max_depth,
        btrfs,
    }
}

pub(crate) struct Recorder<'a> {
    progress: &'a Progress,
}

impl<S> Reporter<S> for Recorder<'_>
where
    S: parallel_disk_usage::size::Size + Into<u64>,
{
    fn report(&self, event: Event<S>) {
        match event {
            Event::ReceiveData(size) => {
                self.progress.entries.fetch_add(1, Ordering::Relaxed);
                self.progress
                    .bytes
                    .fetch_add(size.into(), Ordering::Relaxed);
            }
            Event::EncounterError(report) => {
                let message = format!(
                    "{} {}: {}",
                    report.operation.name(),
                    report.path.display(),
                    report.error
                );
                self.progress.record_error(message);
            }
            _ => {}
        }
    }
}

/// Detaching the scan cannot cancel the blocking pdu traversal.
#[derive(Debug)]
pub struct Scan {
    progress: Arc<Progress>,
    handle: Option<JoinHandle<io::Result<ScanOutcome>>>,
}

impl Scan {
    pub fn spawn(options: ScanOptions) -> io::Result<Self> {
        let progress = Arc::new(Progress::default());
        let worker_progress = Arc::clone(&progress);
        let handle = thread::Builder::new()
            .name("scan".into())
            .spawn(move || scan_reported(&options, &worker_progress))?;
        Ok(Self {
            progress,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> ProgressSnapshot {
        self.progress.snapshot()
    }

    pub fn tree_snapshot(&self) -> Option<Node> {
        self.progress.tree_snapshot()
    }

    pub fn errors(&self) -> Vec<String> {
        self.progress.recent_errors()
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn try_finish(&mut self) -> Option<io::Result<ScanOutcome>> {
        if !self.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(
            handle
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("scan thread panicked"))),
        )
    }

    pub fn wait(mut self) -> io::Result<ScanOutcome> {
        let handle = self.handle.take().expect("handle is taken once");
        handle
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("scan thread panicked")))
    }
}

#[cfg(test)]
mod test {
    use super::{Progress, Scan, ScanOptions, SizeMode, TreeUpdate, scan};

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        std::fs::write(dir.path().join("big"), vec![0u8; 64 * 1024]).expect("write big");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub");
        std::fs::write(dir.path().join("sub/small"), vec![0u8; 1024]).expect("write small");
        dir
    }

    fn options(dir: &tempfile::TempDir) -> ScanOptions {
        let mut options = ScanOptions::new(dir.path());
        options.size_mode = SizeMode::Apparent;
        options
    }

    #[test]
    fn children_are_sorted_largest_first() {
        let dir = fixture();
        let outcome = scan(&options(&dir)).expect("scan");
        let names: Vec<_> = outcome
            .tree
            .children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["big", "sub"]);
        assert_eq!(outcome.tree.child("big").map(|n| n.size), Some(64 * 1024));
        assert_eq!(
            outcome
                .tree
                .child("sub")
                .and_then(|n| n.child("small"))
                .map(|n| n.size),
            Some(1024)
        );
    }

    #[test]
    fn max_depth_keeps_totals_but_drops_children() {
        let dir = fixture();
        let mut options = options(&dir);
        options.max_depth = 2;
        let outcome = scan(&options).expect("scan");
        let sub = outcome.tree.child("sub").expect("sub");
        assert_eq!(sub.children.len(), 0);
        assert!(sub.size >= 1024);

        options.max_depth = 1;
        let outcome = scan(&options).expect("scan");
        assert!(outcome.tree.children.is_empty());
        assert!(outcome.tree.size >= 64 * 1024);
    }

    #[test]
    fn small_nodes_are_culled() {
        let dir = fixture();
        let mut options = options(&dir);
        options.min_ratio = 0.1;
        let outcome = scan(&options).expect("scan");
        assert!(outcome.tree.child("big").is_some());
        assert!(outcome.tree.child("sub").is_none());
    }

    #[test]
    fn missing_root_is_an_error() {
        let mut options = ScanOptions::new("/nonexistent/disk-vivarium");
        options.max_depth = 1;
        assert!(scan(&options).is_err());
    }

    #[test]
    fn spawned_scan_finishes() {
        let dir = fixture();
        let scan = Scan::spawn(options(&dir)).expect("spawn");
        let outcome = scan.wait().expect("scan");
        assert!(outcome.tree.size > 0);
        assert_eq!(outcome.progress.errors, 0);
    }

    #[test]
    fn progress_exposes_an_incomplete_tree() {
        use crate::model::BtrfsUsage;
        use parallel_disk_usage::os_string_display::OsStringDisplay;

        let progress = Progress::default();
        let root = std::path::Path::new("root");
        let children = vec![OsStringDisplay::from(std::ffi::OsString::from("file"))];
        progress.record_tree(
            root,
            "root".into(),
            TreeUpdate {
                size: BtrfsUsage::default(),
                children: &children,
                is_directory: true,
                btrfs: true,
            },
            2,
        );
        let usage = BtrfsUsage {
            disk: 1_024,
            uncompressed: 4_096,
            referenced: 8_192,
        };
        progress.record_tree(
            &root.join("file"),
            "file".into(),
            TreeUpdate {
                size: usage,
                children: &[],
                is_directory: false,
                btrfs: true,
            },
            2,
        );
        let tree = progress.tree_snapshot().expect("partial tree");
        assert!(tree.is_directory);
        assert!(!tree.needs_sub_scan);
        assert_eq!(tree.size, usage.disk);
        assert_eq!(tree.child("file").map(|node| node.size), Some(usage.disk));
        assert_eq!(tree.btrfs, Some(usage));
    }

    #[test]
    fn live_tree_keeps_pruned_sizes() {
        use crate::model::BtrfsUsage;
        use parallel_disk_usage::os_string_display::OsStringDisplay;

        let children = vec![OsStringDisplay::from(std::ffi::OsString::from("file"))];
        let progress = Progress::default();
        progress.record_tree(
            std::path::Path::new("root"),
            "root".into(),
            TreeUpdate {
                size: BtrfsUsage::from(4_096),
                children: &children,
                is_directory: true,
                btrfs: false,
            },
            1,
        );
        progress.record_tree(
            std::path::Path::new("root/file"),
            "file".into(),
            TreeUpdate {
                size: BtrfsUsage::from(1_024),
                children: &[],
                is_directory: false,
                btrfs: false,
            },
            1,
        );
        let tree = progress.tree_snapshot().expect("updated snapshot");
        assert_eq!(tree.size, 5_120);
        assert!(tree.children.is_empty());
        assert!(tree.needs_sub_scan);
    }

    #[test]
    fn sub_scans_reveal_successive_directory_levels() {
        let dir = tempfile::tempdir().expect("temporary directory");
        std::fs::create_dir_all(dir.path().join("one/two")).expect("nested directories");
        std::fs::write(dir.path().join("one/two/file"), vec![0_u8; 1024]).expect("nested file");
        let mut options = ScanOptions::new(dir.path());
        options.max_depth = 2;

        for (directory, child_name) in [("one", "one"), ("one/two", "two")] {
            let outcome = scan(&options).expect("bounded scan");
            let child = outcome.tree.child(child_name).expect("directory entry");
            assert!(child.is_directory);
            assert!(child.needs_sub_scan);
            options.root = dir.path().join(directory);
        }
        let leaf = scan(&options).expect("nested scan");
        assert!(!leaf.tree.child("file").expect("file").is_directory);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cross_device_btrfs_uses_normal_accounting() {
        let root = std::env::current_dir().expect("working directory");
        if !crate::btrfs::is_btrfs(&root).expect("filesystem type") {
            return;
        }
        let mut options = ScanOptions::new(root);
        options.cross_device = true;
        let outcome = scan(&options).expect("normal scan");
        assert!(outcome.btrfs.is_none());
        assert!(
            outcome
                .btrfs_unavailable
                .is_some_and(|reason| reason.contains("Stay on one filesystem"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn btrfs_scan_is_exact_or_has_an_explicit_fallback() {
        let root = std::env::current_dir()
            .expect("working directory")
            .join("src/scan.rs");
        if !crate::btrfs::is_btrfs(&root).expect("filesystem type") {
            return;
        }
        let outcome = scan(&ScanOptions::new(root)).expect("Btrfs scan");
        assert!(outcome.tree.size > 0);
        if let Some(reason) = outcome.btrfs_unavailable {
            assert!(
                reason.contains("CAP_SYS_ADMIN"),
                "unexpected fallback: {reason}"
            );
            assert!(outcome.btrfs.is_none());
        } else {
            assert_eq!(outcome.btrfs.expect("exact totals").files, 1);
        }
    }
}
