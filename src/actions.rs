//! User actions on scanned entries. Disk operations never run on the UI thread.

use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use crate::arena::{Arena, NodeId, ROOT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    Trash,
    Permanent,
}

#[derive(Debug)]
pub struct ActionResult {
    pub id: NodeId,
    pub outcome: Result<(), String>,
}

/// Rebuilds the on-disk path of `id` and refuses targets that must never be deleted.
pub fn target_path(arena: &Arena, id: NodeId) -> io::Result<PathBuf> {
    if id == ROOT {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to delete the scan root",
        ));
    }
    // Lossy names from non-UTF-8 entries rebuild a path that does not exist, so nothing is touched
    let path = PathBuf::from(arena.path_of(id));
    if is_mount_point(&path)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to delete a mount point: its contents were not scanned",
        ));
    }
    Ok(path)
}

/// The scanner shows other filesystems as empty folders, so deleting one would wipe unseen data.
#[cfg(unix)]
fn is_mount_point(path: &Path) -> io::Result<bool> {
    let metadata = std::fs::symlink_metadata(path)?;
    let Some(parent) = path.parent().filter(|_| metadata.is_dir()) else {
        return Ok(false);
    };
    Ok(std::fs::symlink_metadata(parent)?.dev() != metadata.dev())
}

/// On Windows, refuse drive roots and directory volume mount points whose volume root differs
/// from their parent directory's volume root. Regular symlinks and same-volume junctions are kept
/// deletable (only the link/junction itself is removed).
#[cfg(windows)]
fn is_mount_point(path: &Path) -> io::Result<bool> {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

    let metadata = std::fs::symlink_metadata(path)?;
    if (metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY) == 0 {
        return Ok(false);
    }
    if metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let Some(parent) = path.parent() else {
        return Ok(true);
    };
    Ok(!same_volume_root(path, parent)?)
}

#[cfg(windows)]
fn same_volume_root(a: &Path, b: &Path) -> io::Result<bool> {
    let vol_a = volume_path_name(a)?;
    let vol_b = volume_path_name(b)?;
    Ok(vol_a.eq_ignore_ascii_case(&vol_b))
}

#[cfg(windows)]
fn volume_path_name(path: &Path) -> io::Result<String> {
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let cap = wide.len().max(MAX_PATH as usize) + 16;
    let mut out = vec![0_u16; cap];
    // SAFETY: `wide` is NUL-terminated and `out` has capacity `cap` UTF-16 code units.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), out.as_mut_ptr(), out.len() as u32) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = out.iter().position(|&c| c == 0).unwrap_or(out.len());
    Ok(String::from_utf16_lossy(&out[..len]))
}

pub fn open_in_file_manager(path: &Path) -> io::Result<()> {
    let is_directory = std::fs::symlink_metadata(path)?.is_dir();
    let target = if is_directory {
        Some(path)
    } else {
        path.parent()
    };
    let target = target.ok_or_else(|| io::Error::other("path has no parent directory"))?;
    open::that_detached(target)
}

pub fn delete_path(path: &Path, mode: DeleteMode) -> Result<(), String> {
    match mode {
        DeleteMode::Trash => trash::delete(path).map_err(|error| error.to_string()),
        DeleteMode::Permanent => remove_permanently(path).map_err(|error| error.to_string()),
    }
}

#[cfg(unix)]
fn remove_permanently(path: &Path) -> io::Result<()> {
    // `symlink_metadata` keeps a symlink to a directory from being treated as the directory
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(windows)]
fn remove_permanently(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let meta = std::fs::symlink_metadata(path)?;
    let attrs = meta.file_attributes();
    if (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        if (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0 {
            // Directory symlinks and NTFS junctions are removed with `remove_dir` without touching target contents.
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        }
    } else if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Deletes on a worker thread and reports through `results`; `on_done` wakes the UI.
pub fn spawn_delete(
    id: NodeId,
    path: PathBuf,
    mode: DeleteMode,
    results: Sender<ActionResult>,
    on_done: impl FnOnce() + Send + 'static,
) -> io::Result<()> {
    thread::Builder::new()
        .name("inodetree-delete".into())
        .spawn(move || {
            let outcome = delete_path(&path, mode);
            // A closed channel means the app is shutting down, so the result is no longer needed
            let _ = results.send(ActionResult { id, outcome });
            on_done();
        })?;
    Ok(())
}
