//! Parallel directory walker (`getdents64` + `statx` on Linux, `FileIdBothDirectoryInfo` on Windows).

use std::cell::RefCell;
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(unix)]
use std::ffi::{CString, OsString};
use std::io;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use rayon::{Scope, ThreadPoolBuilder};

use super::sys;
#[cfg(unix)]
use super::sys::Metadata;
use super::{HardlinkSet, ScanConfig, ScanProgress, ScanResult};
use crate::arena::{Arena, ChildBatch, NewNode, NodeId, NodeKind, ROOT};

#[cfg(unix)]
const DIRENT_BUFFER_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const WIN_DIRENT_BUFFER_WORDS: usize = 8 * 1024;
// More threads than this make a spinning disk's head jump between directories (thrashing),
// which is slower than a few mostly sequential readers
const HDD_SCAN_THREADS: usize = 3;

#[cfg(unix)]
thread_local! {
    static DIRENT_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; DIRENT_BUFFER_BYTES]);
}

#[cfg(windows)]
thread_local! {
    static WIN_DIRENT_BUFFER: RefCell<Vec<u64>> = RefCell::new(vec![0; WIN_DIRENT_BUFFER_WORDS]);
}

struct Context<'a> {
    arena: Mutex<Arena>,
    hardlinks: HardlinkSet,
    root_dev: u64,
    config: &'a ScanConfig,
}

/// Subdirectories to descend into: batch index and the raw name.
type Subdirs = Vec<(usize, OsString)>;

pub fn scan(config: &ScanConfig) -> io::Result<ScanResult> {
    let started = Instant::now();
    let canonical = std::fs::canonicalize(&config.root)?;
    #[cfg(windows)]
    let root = sys::strip_verbatim_prefix(&canonical);
    #[cfg(unix)]
    let root = canonical;

    #[cfg(unix)]
    let (dev, logical_size, allocated_size, mtime_secs) = {
        let root_meta = sys::statx_path(&root)?;
        if root_meta.file_type() != libc::S_IFDIR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scan root is not a directory",
            ));
        }
        (
            root_meta.dev,
            root_meta.size,
            root_meta.blocks * 512,
            root_meta.mtime_secs,
        )
    };

    #[cfg(windows)]
    let (dev, logical_size, allocated_size, mtime_secs) = {
        let root_meta = sys::stat_root(&root)?;
        (
            root_meta.dev,
            root_meta.logical_size,
            root_meta.allocated_size,
            root_meta.mtime_secs,
        )
    };

    let root_arena = Arena::with_root(
        &root.to_string_lossy(),
        logical_size,
        allocated_size,
        mtime_secs,
    );
    let context = Context {
        arena: Mutex::new(root_arena),
        hardlinks: HardlinkSet::default(),
        root_dev: dev,
        config,
    };
    walk(&context, &root);
    // A poisoned lock still guards a structurally valid arena, since batches are inserted atomically
    let mut arena = context
        .arena
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    arena.aggregate();
    Ok(ScanResult {
        arena,
        elapsed: started.elapsed(),
        was_cancelled: config.cancel.load(Relaxed),
    })
}

fn walk(context: &Context<'_>, root: &Path) {
    #[cfg(unix)]
    let rotational = sys::is_rotational(context.root_dev);
    #[cfg(windows)]
    let rotational = sys::is_rotational(root);

    // If the small pool cannot be built the scan still runs, just on the global pool
    let hdd_pool = rotational
        .then(|| {
            ThreadPoolBuilder::new()
                .num_threads(HDD_SCAN_THREADS)
                .build()
                .ok()
        })
        .flatten();
    match &hdd_pool {
        Some(pool) => pool.scope(|scope| scan_dir(scope, context, root, ROOT)),
        None => rayon::scope(|scope| scan_dir(scope, context, root, ROOT)),
    }
}

fn scan_dir<'scope>(
    scope: &Scope<'scope>,
    context: &'scope Context<'scope>,
    path: &Path,
    id: NodeId,
) {
    if context.config.cancel.load(Relaxed) {
        return;
    }
    let (batch, subdirs) = match read_directory(context, path) {
        Ok(result) => result,
        Err(error) => {
            if error.kind() == io::ErrorKind::PermissionDenied {
                context
                    .config
                    .progress
                    .unreadable_dirs
                    .fetch_add(1, Relaxed);
            }
            return;
        }
    };
    record_progress(&context.config.progress, &batch);
    let first_child = {
        let mut arena = context.arena.lock().unwrap_or_else(PoisonError::into_inner);
        arena.append_children(id, &batch)
    };
    for (index, name) in subdirs {
        let child_path = path.join(name);
        let child_id = first_child + index as NodeId;
        scope.spawn(move |scope| scan_dir(scope, context, &child_path, child_id));
    }
}

#[cfg(unix)]
fn read_directory(context: &Context<'_>, path: &Path) -> io::Result<(ChildBatch, Subdirs)> {
    let dir = sys::open_dir(path)?;
    let mut batch = ChildBatch::default();
    let mut subdirs = Subdirs::new();
    for (_, name) in collect_entries_by_inode(&dir)? {
        // Entries that vanish or fail to stat mid-scan are skipped rather than aborting
        if let Ok(meta) = sys::statx_at(dir.as_raw_fd(), &name, false) {
            add_entry(context, &mut batch, &mut subdirs, name.to_bytes(), &meta);
        }
    }
    Ok((batch, subdirs))
}

#[cfg(windows)]
fn read_directory(context: &Context<'_>, path: &Path) -> io::Result<(ChildBatch, Subdirs)> {
    let mut batch = ChildBatch::default();
    let mut subdirs = Subdirs::new();
    WIN_DIRENT_BUFFER.with(|buffer| {
        sys::read_dir_entries(path, &mut buffer.borrow_mut(), |entry| {
            let is_duplicate = entry.kind == NodeKind::File
                && entry.file_id != 0
                && !context
                    .hardlinks
                    .is_first_sighting(context.root_dev, entry.file_id);
            let (logical_size, allocated_size) = if is_duplicate {
                (0, 0)
            } else {
                (entry.logical_size, entry.allocated_size)
            };
            let name_start = batch.names.len();
            batch.names.push_str(&entry.name.to_string_lossy());
            if entry.should_descend {
                subdirs.push((batch.entries.len(), entry.name));
            }
            batch.entries.push(NewNode {
                name_start,
                name_end: batch.names.len(),
                kind: entry.kind,
                logical_size,
                allocated_size,
                mtime_secs: entry.mtime_secs,
                is_hardlink_duplicate: is_duplicate,
            });
        })
    })?;
    Ok((batch, subdirs))
}

/// Entries sorted by inode number, so the following `statx` calls read the inode table
/// (or NTFS MFT) mostly sequentially instead of seeking at random on spinning disks.
#[cfg(unix)]
fn collect_entries_by_inode(dir: &OwnedFd) -> io::Result<Vec<(u64, CString)>> {
    let mut entries = Vec::new();
    DIRENT_BUFFER.with(|buffer| {
        sys::read_entries(dir.as_fd(), &mut buffer.borrow_mut(), |ino, name| {
            entries.push((ino, name.to_owned()));
        })
    })?;
    entries.sort_unstable_by_key(|(ino, _)| *ino);
    Ok(entries)
}

#[cfg(unix)]
fn add_entry(
    context: &Context<'_>,
    batch: &mut ChildBatch,
    subdirs: &mut Subdirs,
    raw_name: &[u8],
    meta: &Metadata,
) {
    let kind = match meta.file_type() {
        libc::S_IFDIR => NodeKind::Directory,
        libc::S_IFREG => NodeKind::File,
        libc::S_IFLNK => NodeKind::Symlink,
        _ => NodeKind::Other,
    };
    let is_duplicate = kind != NodeKind::Directory
        && meta.nlink > 1
        && !context.hardlinks.is_first_sighting(meta.dev, meta.ino);
    let (logical_size, allocated_size) = if is_duplicate {
        (0, 0)
    } else {
        (meta.size, meta.blocks * 512)
    };
    let name_start = batch.names.len();
    batch.names.push_str(&String::from_utf8_lossy(raw_name));
    // Directories on another device (mount points) stay as empty leaves instead of being entered
    if kind == NodeKind::Directory && meta.dev == context.root_dev {
        subdirs.push((batch.entries.len(), OsString::from_vec(raw_name.to_vec())));
    }
    batch.entries.push(NewNode {
        name_start,
        name_end: batch.names.len(),
        kind,
        logical_size,
        allocated_size,
        mtime_secs: meta.mtime_secs,
        is_hardlink_duplicate: is_duplicate,
    });
}

fn record_progress(progress: &ScanProgress, batch: &ChildBatch) {
    let dirs = batch
        .entries
        .iter()
        .filter(|entry| entry.kind == NodeKind::Directory)
        .count();
    let allocated: u64 = batch.entries.iter().map(|entry| entry.allocated_size).sum();
    progress.dirs.fetch_add(dirs as u64, Relaxed);
    progress
        .files
        .fetch_add((batch.entries.len() - dirs) as u64, Relaxed);
    progress.allocated_bytes.fetch_add(allocated, Relaxed);
}
