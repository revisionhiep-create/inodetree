//! Thin wrappers over the Win32 APIs used by the Windows scanner.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_LEVEL, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES,
    ERROR_NOT_SUPPORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SPARSE_FILE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, FileIdBothDirectoryInfo, FindClose,
    FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    GetCompressedFileSizeW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    INVALID_FILE_SIZE, OPEN_EXISTING, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
};
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

use crate::arena::NodeKind;

const FILETIME_UNIX_EPOCH_SECS: i64 = 11_644_473_600;
const FILETIME_TICKS_PER_SEC: i64 = 10_000_000;

#[derive(Debug, Clone, Copy)]
pub struct RootMeta {
    pub dev: u64,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub mtime_secs: i64,
}

#[derive(Debug)]
pub struct WinDirEntry {
    pub name: OsString,
    pub kind: NodeKind,
    pub should_descend: bool,
    pub file_id: u64,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub mtime_secs: i64,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: `self.0` is a valid Win32 handle owned exclusively by this wrapper.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct FindHandle(HANDLE);

impl Drop for FindHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: `self.0` was returned by `FindFirstFileExW` and is owned by this wrapper.
            unsafe {
                FindClose(self.0);
            }
        }
    }
}

/// Strips `\\?\` and `\\?\UNC\` prefixes returned by `std::fs::canonicalize` so UI paths stay readable.
#[must_use]
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.as_os_str();
    let wide: Vec<u16> = raw.encode_wide().collect();
    let unc_prefix: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
    let verbatim_prefix: Vec<u16> = "\\\\?\\".encode_utf16().collect();
    if wide.starts_with(&unc_prefix) {
        let mut unc = "\\\\".encode_utf16().collect::<Vec<u16>>();
        unc.extend_from_slice(&wide[unc_prefix.len()..]);
        return PathBuf::from(OsString::from_wide(&unc));
    }
    if wide.starts_with(&verbatim_prefix) {
        return PathBuf::from(OsString::from_wide(&wide[verbatim_prefix.len()..]));
    }
    path.to_path_buf()
}

/// Encodes `path` as NUL-terminated UTF-16 with `\\?\` extended-length prefix when needed.
#[must_use]
pub fn to_extended_wide(path: &Path) -> Vec<u16> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let verbatim: Vec<u16> = "\\\\?\\".encode_utf16().collect();
    let unc: Vec<u16> = "\\\\".encode_utf16().collect();

    let mut out = if wide.starts_with(&verbatim) {
        wide
    } else if wide.starts_with(&unc) {
        let mut prefixed: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
        prefixed.extend_from_slice(&wide[2..]);
        prefixed
    } else if wide.len() >= 3 && wide[1] == u16::from(b':') && wide[2] == u16::from(b'\\') {
        let mut prefixed = verbatim;
        prefixed.extend_from_slice(&wide);
        prefixed
    } else {
        wide
    };
    out.push(0);
    out
}

fn to_extended_glob_wide(path: &Path) -> Vec<u16> {
    let mut wide = to_extended_wide(path);
    // Pop trailing NUL so we can append `\*` (or `*` if already ending in `\`).
    wide.pop();
    if !wide.ends_with(&[u16::from(b'\\')]) {
        wide.push(u16::from(b'\\'));
    }
    wide.push(u16::from(b'*'));
    wide.push(0);
    wide
}

const fn filetime_to_unix_secs(ticks: i64) -> i64 {
    (ticks / FILETIME_TICKS_PER_SEC) - FILETIME_UNIX_EPOCH_SECS
}

const fn filetime_parts_to_unix_secs(low: u32, high: u32) -> i64 {
    let ticks = ((high as i64) << 32) | (low as i64);
    filetime_to_unix_secs(ticks)
}

/// Reads metadata for the scan root, verifying it is a directory and following a root symlink.
pub fn stat_root(path: &Path) -> io::Result<RootMeta> {
    let wide = to_extended_wide(path);
    // SAFETY: `wide` is NUL-terminated UTF-16.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);

    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `handle.0` is valid and `info` points to writable `BY_HANDLE_FILE_INFORMATION`.
    let ok = unsafe { GetFileInformationByHandle(handle.0, info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `GetFileInformationByHandle` succeeded and fully initialized `info`.
    let info = unsafe { info.assume_init() };
    if (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scan root is not a directory",
        ));
    }
    Ok(RootMeta {
        dev: u64::from(info.dwVolumeSerialNumber),
        logical_size: 0,
        allocated_size: 0,
        mtime_secs: filetime_parts_to_unix_secs(
            info.ftLastWriteTime.dwLowDateTime,
            info.ftLastWriteTime.dwHighDateTime,
        ),
    })
}

/// Queries whether the drive backing `root` incurs a seek penalty (spinning HDD).
#[must_use]
pub fn is_rotational(root: &Path) -> bool {
    let clean = strip_verbatim_prefix(root);
    let text = clean.to_string_lossy();
    let bytes = text.as_bytes();
    let Some(&drive) = bytes.first() else {
        return false;
    };
    if !drive.is_ascii_alphabetic() || bytes.get(1) != Some(&b':') {
        return false;
    }
    let device_path = format!("\\\\.\\{}:", char::from(drive));
    let wide: Vec<u16> = device_path.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated UTF-16.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let handle = OwnedHandle(handle);

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut descriptor = std::mem::MaybeUninit::<DEVICE_SEEK_PENALTY_DESCRIPTOR>::uninit();
    let mut returned = 0_u32;
    // SAFETY: `query` and `descriptor` are valid for their respective struct sizes.
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            ptr::from_ref(&query).cast(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            descriptor.as_mut_ptr().cast(),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return false;
    }
    // SAFETY: `DeviceIoControl` succeeded and initialized `descriptor`.
    let descriptor = unsafe { descriptor.assume_init() };
    descriptor.IncursSeekPenalty != 0
}

/// Reads all directory entries in `path` via batched `GetFileInformationByHandleEx`
/// (`FileIdBothDirectoryInfo`), falling back to `FindFirstFileExW` on unsupported filesystems.
pub fn read_dir_entries(
    path: &Path,
    buffer: &mut [u64],
    mut visit: impl FnMut(WinDirEntry),
) -> io::Result<()> {
    let wide = to_extended_wide(path);
    // SAFETY: `wide` is NUL-terminated UTF-16.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);

    let byte_len = u32::try_from(size_of_val(buffer)).unwrap_or(u32::MAX);
    let mut emitted_any = false;

    loop {
        // SAFETY: `buffer` is 8-byte aligned and writable for `byte_len` bytes.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                byte_len,
            )
        };
        if ok == 0 {
            // SAFETY: `GetFileInformationByHandleEx` failed; retrieve the thread's last Win32 error.
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_FILES {
                return Ok(());
            }
            if !emitted_any
                && matches!(
                    err,
                    ERROR_INVALID_PARAMETER | ERROR_INVALID_LEVEL | ERROR_NOT_SUPPORTED
                )
            {
                return read_dir_entries_find(path, &mut visit);
            }
            return Err(io::Error::from_raw_os_error(
                i32::try_from(err).unwrap_or(i32::MAX),
            ));
        }

        emitted_any = true;
        let mut offset = 0_usize;
        loop {
            debug_assert_eq!(offset % align_of::<FILE_ID_BOTH_DIR_INFO>(), 0);
            // SAFETY: `buffer` is `u64`-aligned and `NextEntryOffset` advances on 8-byte boundaries.
            let entry = unsafe {
                &*(buffer
                    .as_ptr()
                    .add(offset / 8)
                    .cast::<FILE_ID_BOTH_DIR_INFO>())
            };
            let name_len = (entry.FileNameLength as usize) / 2;
            // SAFETY: `FileName` holds `name_len` UTF-16 code units right after the fixed header.
            let name_slice = unsafe {
                std::slice::from_raw_parts(ptr::addr_of!(entry.FileName).cast::<u16>(), name_len)
            };
            if !is_dot_or_dotdot(name_slice) {
                let name = OsString::from_wide(name_slice);
                let (kind, should_descend) = classify_entry(entry.FileAttributes, entry.EaSize);
                let (logical_size, allocated_size) = if kind == NodeKind::File {
                    let logical = entry.EndOfFile.max(0) as u64;
                    let alloc = file_allocated_size(
                        path,
                        &name,
                        entry.FileAttributes,
                        entry.AllocationSize.max(0) as u64,
                    );
                    (logical, alloc)
                } else {
                    (0, 0)
                };
                visit(WinDirEntry {
                    name,
                    kind,
                    should_descend,
                    file_id: entry.FileId as u64,
                    logical_size,
                    allocated_size,
                    mtime_secs: filetime_to_unix_secs(entry.LastWriteTime),
                });
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
    }
}

fn read_dir_entries_find(path: &Path, visit: &mut impl FnMut(WinDirEntry)) -> io::Result<()> {
    let pattern = to_extended_glob_wide(path);
    let mut data = std::mem::MaybeUninit::<WIN32_FIND_DATAW>::uninit();
    // SAFETY: `pattern` is NUL-terminated UTF-16 and `data` points to writable `WIN32_FIND_DATAW`.
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            data.as_mut_ptr().cast(),
            FindExSearchNameMatch,
            ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: `FindFirstFileExW` failed; read last error immediately.
        let err = unsafe { GetLastError() };
        if err == ERROR_NO_MORE_FILES {
            return Ok(());
        }
        return Err(io::Error::from_raw_os_error(
            i32::try_from(err).unwrap_or(i32::MAX),
        ));
    }
    let handle = FindHandle(handle);

    loop {
        // SAFETY: `FindFirstFileExW` / `FindNextFileW` succeeded and initialized `data`.
        let entry = unsafe { data.assume_init() };
        let name_len = entry
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.cFileName.len());
        let name_slice = &entry.cFileName[..name_len];
        if !is_dot_or_dotdot(name_slice) {
            let name = OsString::from_wide(name_slice);
            let (kind, should_descend) = classify_entry(entry.dwFileAttributes, entry.dwReserved0);
            let (logical_size, allocated_size) = if kind == NodeKind::File {
                let logical =
                    (u64::from(entry.nFileSizeHigh) << 32) | u64::from(entry.nFileSizeLow);
                let alloc = file_allocated_size(path, &name, entry.dwFileAttributes, logical);
                (logical, alloc)
            } else {
                (0, 0)
            };
            visit(WinDirEntry {
                name,
                kind,
                should_descend,
                file_id: 0,
                logical_size,
                allocated_size,
                mtime_secs: filetime_parts_to_unix_secs(
                    entry.ftLastWriteTime.dwLowDateTime,
                    entry.ftLastWriteTime.dwHighDateTime,
                ),
            });
        }
        // SAFETY: `handle.0` is a valid search handle and `data` is writable.
        let has_next = unsafe { FindNextFileW(handle.0, data.as_mut_ptr()) };
        if has_next == 0 {
            // SAFETY: `FindNextFileW` returned 0; check whether it reached end of directory.
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_FILES {
                return Ok(());
            }
            return Err(io::Error::from_raw_os_error(
                i32::try_from(err).unwrap_or(i32::MAX),
            ));
        }
    }
}

fn is_dot_or_dotdot(name: &[u16]) -> bool {
    name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')]
}

const fn classify_entry(attrs: u32, reparse_tag: u32) -> (NodeKind, bool) {
    let is_reparse = (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0;
    let is_dir = (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
    if is_reparse && reparse_tag == IO_REPARSE_TAG_SYMLINK {
        (NodeKind::Symlink, false)
    } else if is_reparse && reparse_tag == IO_REPARSE_TAG_MOUNT_POINT {
        // Junctions and volume mount points remain empty leaves so scans never loop or cross volumes.
        (NodeKind::Directory, false)
    } else if is_dir {
        (NodeKind::Directory, true)
    } else {
        (NodeKind::File, false)
    }
}

fn file_allocated_size(dir: &Path, name: &OsString, attrs: u32, fallback_alloc: u64) -> u64 {
    // Cloud-only placeholder files (e.g. OneDrive Files On-Demand) occupy no local disk space
    // and must not be hydrated by opening or querying them.
    if (attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)) != 0 {
        return 0;
    }
    if (attrs & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED)) != 0 {
        return query_compressed_size(&dir.join(name)).unwrap_or(fallback_alloc);
    }
    fallback_alloc
}

fn query_compressed_size(path: &Path) -> Option<u64> {
    let wide = to_extended_wide(path);
    let mut high = 0_u32;
    // SAFETY: `wide` is NUL-terminated UTF-16 and `high` is a writable `u32`.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
    // SAFETY: `GetLastError` disambiguates a genuine 4 GiB-aligned size from an error.
    if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != 0 {
        return None;
    }
    Some((u64::from(high) << 32) | u64::from(low))
}
