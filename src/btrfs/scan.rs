//! `TreeBuilder` supplies file paths for Btrfs ioctls, unlike metadata-only size walkers.

use std::{
    fs::{File, read_dir, symlink_metadata},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use parallel_disk_usage::{
    data_tree::DataTree,
    os_string_display::OsStringDisplay,
    reporter::{ErrorReport, Event, Reporter, error_report::Operation},
    tree_builder::{Info, TreeBuilder},
};

use crate::{
    model::BtrfsUsage,
    scan::{Progress, Recorder, ScanOptions, ScanOutcome, TreeUpdate, finish_tree},
};

use super::Scanner;

pub(crate) enum BtrfsScan {
    Complete(Box<ScanOutcome>),
    Unavailable(String),
}

pub(crate) fn scan(
    options: &ScanOptions,
    progress: &Progress,
    reporter: &Recorder,
) -> io::Result<BtrfsScan> {
    let metadata = symlink_metadata(&options.root)?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Ok(BtrfsScan::Unavailable(
            "the scan root is not a regular file or directory".into(),
        ));
    }

    // Check privileges before starting the parallel walk so a denied scan never mixes metrics.
    let root_file = open_file(&options.root)?;
    let preflight = Scanner::new();
    if let Err(error) = preflight.file_usage(&root_file, metadata.dev(), metadata.ino()) {
        return Ok(BtrfsScan::Unavailable(format!(
            "exact compression and sharing metrics require root or CAP_SYS_ADMIN: {error}"
        )));
    }

    let scanner = Scanner::new();
    let context = Context {
        root: &options.root,
        root_device: metadata.dev(),
        stay: (!options.cross_device).then_some(metadata.dev()),
        max_depth: options.max_depth,
        scanner: &scanner,
        progress,
        reporter,
    };
    let builder = TreeBuilder {
        name: OsStringDisplay::os_string_from(&options.root),
        path: options.root.clone(),
        get_info: |path| info(path, &context),
        join_path: |prefix, name| prefix.join(&name.0),
        max_depth: options.max_depth,
    };
    let data_tree: DataTree<OsStringDisplay, BtrfsUsage> = builder.into();
    Ok(BtrfsScan::Complete(Box::new(finish_tree(
        data_tree,
        options,
        progress,
        Some(scanner.report()),
        None,
    ))))
}

struct Context<'a> {
    root: &'a Path,
    root_device: u64,
    stay: Option<u64>,
    max_depth: u64,
    scanner: &'a Scanner,
    progress: &'a Progress,
    reporter: &'a Recorder<'a>,
}

fn info(path: &PathBuf, context: &Context<'_>) -> Info<OsStringDisplay, BtrfsUsage> {
    if context.progress.cancellation_requested() {
        return Info::default();
    }
    let stats = match symlink_metadata(path) {
        Ok(stats) => stats,
        Err(error) => {
            report_fs_error(context.reporter, Operation::SymlinkMetadata, path, error);
            return Info::default();
        }
    };
    let is_dir = stats.is_dir();
    let same_device = context.stay.is_none_or(|device| device == stats.dev());

    let mut size = BtrfsUsage::from(stats.blocks() * 512);
    if stats.is_file() && stats.dev() == context.root_device {
        match open_file(path)
            .and_then(|file| context.scanner.file_usage(&file, stats.dev(), stats.ino()))
        {
            Ok(Some(usage)) => size = usage,
            Ok(None) => {}
            Err(error) => context
                .progress
                .record_error(format!("Btrfs metadata {}: {error}", path.display())),
        }
    }

    context.reporter.report(Event::ReceiveData(size));
    let children = if is_dir && same_device {
        match read_dir(path) {
            Ok(entries) => entries
                .take_while(|_| !context.progress.cancellation_requested())
                .filter_map(|entry| match entry {
                    Ok(entry) => Some(OsStringDisplay::from(entry.file_name())),
                    Err(error) => {
                        report_fs_error(context.reporter, Operation::AccessEntry, path, error);
                        None
                    }
                })
                .collect(),
            Err(error) => {
                report_fs_error(context.reporter, Operation::ReadDirectory, path, error);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let name = if path.as_path() == context.root {
        context.root.to_string_lossy().into_owned()
    } else {
        path.file_name()
            .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
            .into_owned()
    };
    context.progress.record_tree(
        path,
        name,
        TreeUpdate {
            size,
            children: &children,
            is_directory: is_dir,
            btrfs: true,
        },
        context.max_depth,
    );
    Info { size, children }
}

fn report_fs_error(reporter: &Recorder, operation: Operation, path: &Path, error: io::Error) {
    let event: Event<BtrfsUsage> = Event::EncounterError(ErrorReport {
        operation,
        path,
        error,
    });
    reporter.report(event);
}

fn open_file(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}
