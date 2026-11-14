//! Physical extents are deduplicated per device to match `compsize` without a second walk.

#![cfg(target_os = "linux")]

mod scan;
pub(crate) use scan::{BtrfsScan, scan};

use std::{
    cell::RefCell,
    collections::HashSet,
    ffi::CString,
    fs::File,
    io,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(test)]
use std::mem::offset_of;

use crate::model::{BtrfsReport, BtrfsUsage, Compression};

const BTRFS_SUPER_MAGIC: libc::c_ulong = 0x9123_683e;
const EXTENT_DATA_KEY: u32 = 108;
const FILE_EXTENT_INLINE: u8 = 0;
const FILE_EXTENT_REGULAR: u8 = 1;
const FILE_EXTENT_PREALLOC: u8 = 2;
const SEARCH_BUFFER_SIZE: usize = 65_536;
const SEARCH_HEADER_SIZE: usize = 32;
const FILE_EXTENT_ITEM_SIZE: usize = 53;
const INLINE_HEADER_SIZE: usize = 21;
const LEDGER_SHARDS: usize = 64;
#[cfg(test)]
const HEADER_SIZE: usize = offset_of!(SearchArgs, buf);

#[repr(C)]
#[derive(Debug, Default)]
struct SearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    unused: u32,
    unused1: u64,
    unused2: u64,
    unused3: u64,
    unused4: u64,
}

/// The array must follow the header inline, as it does in the kernel's flexible buffer.
#[repr(C)]
struct SearchArgs {
    key: SearchKey,
    buf_size: u64,
    buf: [u8; SEARCH_BUFFER_SIZE],
}

impl SearchArgs {
    fn new(inode: u64) -> Self {
        Self {
            key: search_key(inode),
            buf_size: SEARCH_BUFFER_SIZE as u64,
            buf: [0; SEARCH_BUFFER_SIZE],
        }
    }
}

fn search_key(inode: u64) -> SearchKey {
    SearchKey {
        tree_id: 0,
        min_objectid: inode,
        max_objectid: inode,
        min_offset: 0,
        max_offset: u64::MAX,
        min_transid: 0,
        max_transid: u64::MAX,
        min_type: EXTENT_DATA_KEY,
        max_type: EXTENT_DATA_KEY,
        nr_items: u32::MAX,
        unused: 0,
        unused1: 0,
        unused2: 0,
        unused3: 0,
        unused4: 0,
    }
}

/// The ioctl header omits the inline search buffer from its size.
#[repr(C)]
struct SearchPrefix {
    key: SearchKey,
    buf_size: u64,
}

const TREE_SEARCH_V2: libc::c_ulong = libc::_IOWR::<SearchPrefix>(0x94, 17);

pub fn is_btrfs(path: &Path) -> io::Result<bool> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `path` is NUL-terminated and `stat` points to valid writable storage.
    let mut stat = unsafe { std::mem::zeroed::<libc::statfs>() };
    // SAFETY: `stat` is initialized by a successful statfs call.
    let result = unsafe { libc::statfs(path.as_ptr(), &mut stat) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_type as libc::c_ulong == BTRFS_SUPER_MAGIC)
}

#[derive(Debug)]
pub struct Scanner {
    ledger: Ledger,
    files: AtomicU64,
    unique_extents: AtomicU64,
    totals: [AtomicUsage; 5],
    disabled: AtomicBool,
}

#[derive(Debug, Default)]
struct AtomicUsage {
    disk: AtomicU64,
    uncompressed: AtomicU64,
    referenced: AtomicU64,
}

thread_local! {
    static SEARCH_ARGS: RefCell<Option<Box<SearchArgs>>> = const { RefCell::new(None) };
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            ledger: Ledger::new(),
            files: AtomicU64::new(0),
            unique_extents: AtomicU64::new(0),
            totals: std::array::from_fn(|_| AtomicUsage::default()),
            disabled: AtomicBool::new(false),
        }
    }

    pub fn file_usage(
        &self,
        file: &File,
        device: u64,
        inode: u64,
    ) -> io::Result<Option<BtrfsUsage>> {
        SEARCH_ARGS.with(|buffer| {
            let mut buffer = buffer.borrow_mut();
            let mut args = buffer
                .take()
                .unwrap_or_else(|| Box::new(SearchArgs::new(inode)));
            args.key = search_key(inode);
            args.buf_size = SEARCH_BUFFER_SIZE as u64;
            let result = self.file_usage_buffered(file, device, &mut args);
            *buffer = Some(args);
            result
        })
    }

    fn file_usage_buffered(
        &self,
        file: &File,
        device: u64,
        args: &mut SearchArgs,
    ) -> io::Result<Option<BtrfsUsage>> {
        if self.disabled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut extents = Vec::new();
        loop {
            args.key.nr_items = u32::MAX;
            // SAFETY: the fd is live and the header is followed by the requested buffer.
            let result = unsafe { libc::ioctl(file.as_raw_fd(), TREE_SEARCH_V2, &mut *args) };
            if result == -1 {
                let error = io::Error::last_os_error();
                if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES)) {
                    self.disabled.store(true, Ordering::Relaxed);
                }
                return Err(error);
            }
            let count = args.key.nr_items as usize;
            if count == 0 {
                break;
            }
            let mut cursor = 0;
            let mut last_offset = 0;
            for _ in 0..count {
                let header: &[u8] = args
                    .buf
                    .get(cursor..cursor + SEARCH_HEADER_SIZE)
                    .ok_or_else(|| invalid("truncated search header"))?;
                last_offset = u64_at(header, 16)?;
                let length = u32_at(header, 28)? as usize;
                let payload_start = cursor + SEARCH_HEADER_SIZE;
                let payload_end = payload_start
                    .checked_add(length)
                    .filter(|end| *end <= args.buf_size as usize)
                    .ok_or_else(|| invalid("extent record exceeds search buffer"))?;
                if let Some(extent) = parse_extent(&args.buf[payload_start..payload_end])? {
                    extents.push((extent, device));
                }
                cursor = payload_end;
            }
            if !advance_search(&mut args.key, last_offset) {
                break;
            }
        }
        self.files.fetch_add(1, Ordering::Relaxed);
        let total = extents
            .into_iter()
            .map(|(extent, device)| self.record(extent, device))
            .sum();
        Ok(Some(total))
    }

    fn record(&self, extent: Extent, device: u64) -> BtrfsUsage {
        let unique = match extent.physical {
            Some(address) => self.ledger.insert(device, address),
            None => true,
        };
        let usage = BtrfsUsage {
            disk: if unique { extent.disk } else { 0 },
            uncompressed: if unique { extent.uncompressed } else { 0 },
            referenced: extent.referenced,
        };
        if unique {
            self.unique_extents.fetch_add(1, Ordering::Relaxed);
        }
        self.totals[extent.compression].add(usage);
        usage
    }

    pub fn report(&self) -> BtrfsReport {
        let mut total = BtrfsUsage::default();
        let by_compression = self
            .totals
            .iter()
            .enumerate()
            .filter_map(|(index, usage)| {
                let usage = usage.load();
                (usage != BtrfsUsage::default()).then_some((compression(index), usage))
            })
            .inspect(|(_, usage)| total += *usage)
            .collect();
        BtrfsReport {
            files: self.files.load(Ordering::Relaxed),
            unique_extents: self.unique_extents.load(Ordering::Relaxed),
            total,
            by_compression,
        }
    }
}

impl AtomicUsage {
    fn add(&self, usage: BtrfsUsage) {
        self.disk.fetch_add(usage.disk, Ordering::Relaxed);
        self.uncompressed
            .fetch_add(usage.uncompressed, Ordering::Relaxed);
        self.referenced
            .fetch_add(usage.referenced, Ordering::Relaxed);
    }

    fn load(&self) -> BtrfsUsage {
        BtrfsUsage {
            disk: self.disk.load(Ordering::Relaxed),
            uncompressed: self.uncompressed.load(Ordering::Relaxed),
            referenced: self.referenced.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct Ledger {
    shards: Vec<Mutex<HashSet<(u64, u64)>>>,
}

impl Ledger {
    fn new() -> Self {
        Self {
            shards: (0..LEDGER_SHARDS)
                .map(|_| Mutex::new(HashSet::new()))
                .collect(),
        }
    }

    fn insert(&self, device: u64, address: u64) -> bool {
        let key = (device, address);
        let shard = (address >> 12) as usize % self.shards.len();
        self.shards[shard]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    compression: usize,
    physical: Option<u64>,
    disk: u64,
    uncompressed: u64,
    referenced: u64,
}

fn parse_extent(item: &[u8]) -> io::Result<Option<Extent>> {
    if item.len() < INLINE_HEADER_SIZE {
        return Err(invalid("short Btrfs file extent item"));
    }
    let kind = item[20];
    if kind != FILE_EXTENT_INLINE && item.len() != FILE_EXTENT_ITEM_SIZE {
        return Err(invalid("unexpected regular Btrfs extent size"));
    }
    let compression = Compression::from_raw(item[16], kind == FILE_EXTENT_PREALLOC) as usize;
    let referenced = u64_at(item, 8)?;
    if kind == FILE_EXTENT_INLINE {
        return Ok(Some(Extent {
            compression,
            physical: None,
            disk: (item.len() - INLINE_HEADER_SIZE) as u64,
            uncompressed: referenced,
            referenced,
        }));
    }
    if kind != FILE_EXTENT_REGULAR && kind != FILE_EXTENT_PREALLOC {
        return Err(invalid("unknown Btrfs file extent type"));
    }
    let physical = u64_at(item, 21)?;
    if physical == 0 {
        return Ok(None);
    }
    Ok(Some(Extent {
        compression,
        physical: Some(physical),
        disk: u64_at(item, 29)?,
        uncompressed: u64_at(item, 8)?,
        referenced: u64_at(item, 45)?,
    }))
}

const fn compression(index: usize) -> Compression {
    match index {
        0 => Compression::None,
        1 => Compression::Zlib,
        2 => Compression::Lzo,
        3 => Compression::Zstd,
        _ => Compression::Other,
    }
}

/// The kernel returns the item count through the same field used for the next request's limit.
fn advance_search(key: &mut SearchKey, last_offset: u64) -> bool {
    let Some(next_offset) = last_offset.checked_add(1) else {
        return false;
    };
    key.min_offset = next_offset;
    key.nr_items = u32::MAX;
    true
}

fn u64_at(bytes: &[u8], offset: usize) -> io::Result<u64> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok().map(u64::from_le_bytes))
        .ok_or_else(|| invalid("truncated 64-bit extent field"))
}

fn u32_at(bytes: &[u8], offset: usize) -> io::Result<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok().map(u32::from_le_bytes))
        .ok_or_else(|| invalid("truncated 32-bit search field"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular_extent(address: u64, disk: u64, uncompressed: u64, referenced: u64) -> [u8; 53] {
        let mut bytes = [0; 53];
        bytes[8..16].copy_from_slice(&uncompressed.to_le_bytes());
        bytes[16] = 3;
        bytes[20] = FILE_EXTENT_REGULAR;
        bytes[21..29].copy_from_slice(&address.to_le_bytes());
        bytes[29..37].copy_from_slice(&disk.to_le_bytes());
        bytes[45..53].copy_from_slice(&referenced.to_le_bytes());
        bytes
    }

    #[test]
    fn reflinked_extents_contribute_physical_space_once() {
        let scanner = Scanner::new();
        let device = 42;
        let extent = parse_extent(&regular_extent(4096, 1024, 8192, 8192))
            .expect("valid extent")
            .expect("not a hole");
        let first = scanner.record(extent, device);
        let second = scanner.record(extent, device);
        assert_eq!(
            first,
            BtrfsUsage {
                disk: 1024,
                uncompressed: 8192,
                referenced: 8192
            }
        );
        assert_eq!(
            second,
            BtrfsUsage {
                disk: 0,
                uncompressed: 0,
                referenced: 8192
            }
        );
        let report = scanner.report();
        assert_eq!(report.unique_extents, 1);
        assert_eq!(report.total, first + second);
        assert_eq!(
            report.by_compression,
            vec![(Compression::Zstd, first + second)]
        );
    }

    #[test]
    fn same_address_on_another_device_is_not_a_reflink() {
        let scanner = Scanner::new();
        let extent = parse_extent(&regular_extent(4096, 1024, 8192, 8192))
            .expect("valid extent")
            .expect("not a hole");
        assert_eq!(scanner.record(extent, 1).disk, 1024);
        assert_eq!(scanner.record(extent, 2).disk, 1024);
    }

    #[test]
    fn holes_have_no_usage_and_inline_data_is_unique() {
        let hole = regular_extent(0, 0, 4096, 4096);
        assert_eq!(parse_extent(&hole).expect("hole"), None);
        let mut inline = vec![0; INLINE_HEADER_SIZE + 11];
        inline[8..16].copy_from_slice(&11_u64.to_le_bytes());
        inline[20] = FILE_EXTENT_INLINE;
        let usage = Scanner::new().record(
            parse_extent(&inline).expect("valid inline").expect("usage"),
            1,
        );
        assert_eq!(
            usage,
            BtrfsUsage {
                disk: 11,
                uncompressed: 11,
                referenced: 11
            }
        );
    }

    #[test]
    fn uapi_layout_matches_the_kernel_structure() {
        assert_eq!(HEADER_SIZE, 112);
        assert_eq!(offset_of!(SearchKey, nr_items), 64);
        assert_eq!(std::mem::size_of::<SearchKey>(), 104);
        assert_eq!(std::mem::size_of::<SearchPrefix>(), 112);
        assert_eq!(
            std::mem::size_of::<SearchArgs>(),
            HEADER_SIZE + SEARCH_BUFFER_SIZE
        );
    }

    #[test]
    fn search_restores_requested_page_capacity() {
        let mut key = search_key(42);
        key.nr_items = 1;
        assert!(advance_search(&mut key, 253));
        assert_eq!(key.min_offset, 254);
        assert_eq!(key.nr_items, u32::MAX);
        assert!(!advance_search(&mut key, u64::MAX));
    }
}
