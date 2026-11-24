//! Thin wrappers over the raw Linux syscalls used by the scanner.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const STATX_MASK: u32 = libc::STATX_TYPE
    | libc::STATX_MODE
    | libc::STATX_NLINK
    | libc::STATX_SIZE
    | libc::STATX_BLOCKS
    | libc::STATX_MTIME
    | libc::STATX_INO;

// Layout of `linux_dirent64`: ino (8) + off (8) + reclen (2) + type (1) + NUL-terminated name
const DIRENT_INO_OFFSET: usize = 0;
const DIRENT_RECLEN_OFFSET: usize = 16;
const DIRENT_NAME_OFFSET: usize = 19;

#[derive(Debug, Clone, Copy)]
pub struct Metadata {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
    pub mode: u32,
    pub size: u64,
    pub blocks: u64,
    pub mtime_secs: i64,
}

impl Metadata {
    #[must_use]
    pub fn file_type(&self) -> u32 {
        self.mode & libc::S_IFMT
    }
}

pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c_path` is a valid NUL-terminated string for the duration of the call
    let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` just returned this descriptor and nothing else owns it
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `follow_symlinks` is only meant for the scan root. Entries are never followed.
pub fn statx_at(dirfd: i32, name: &CStr, follow_symlinks: bool) -> io::Result<Metadata> {
    let nofollow = if follow_symlinks {
        0
    } else {
        libc::AT_SYMLINK_NOFOLLOW
    };
    let flags = nofollow | libc::AT_STATX_DONT_SYNC;
    let mut buffer = std::mem::MaybeUninit::<libc::statx>::uninit();
    // SAFETY: `name` is NUL-terminated and `buffer` is a valid out-pointer for one `statx`
    let result =
        unsafe { libc::statx(dirfd, name.as_ptr(), flags, STATX_MASK, buffer.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statx` returned 0, so it fully initialized the struct
    let raw = unsafe { buffer.assume_init() };
    Ok(Metadata {
        dev: libc::makedev(raw.stx_dev_major, raw.stx_dev_minor),
        ino: raw.stx_ino,
        nlink: raw.stx_nlink,
        mode: u32::from(raw.stx_mode),
        size: raw.stx_size,
        blocks: raw.stx_blocks,
        mtime_secs: raw.stx_mtime.tv_sec,
    })
}

pub fn statx_path(path: &Path) -> io::Result<Metadata> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    statx_at(libc::AT_FDCWD, &c_path, true)
}

/// Whether the block device behind `dev` reports itself as rotational (a spinning HDD).
///
/// Devices without a sysfs entry, such as the anonymous ones of btrfs or ZFS, count as
/// non-rotational.
#[must_use]
pub fn is_rotational(dev: u64) -> bool {
    let device_dir = format!("/sys/dev/block/{}:{}", libc::major(dev), libc::minor(dev));
    // Partitions have no queue of their own, so their parent disk is consulted instead
    ["queue/rotational", "../queue/rotational"]
        .iter()
        .find_map(|relative| std::fs::read(format!("{device_dir}/{relative}")).ok())
        .is_some_and(|content| content.first() == Some(&b'1'))
}

/// Calls `visit` with the inode number and name of every entry except `.` and `..`,
/// reading with `getdents64`.
pub fn read_entries(
    dir: BorrowedFd<'_>,
    buffer: &mut [u8],
    mut visit: impl FnMut(u64, &CStr),
) -> io::Result<()> {
    loop {
        // SAFETY: the kernel writes at most `buffer.len()` bytes into `buffer`
        let filled = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                dir.as_raw_fd(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if filled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if filled == 0 {
            return Ok(());
        }
        visit_records(&buffer[..filled as usize], &mut visit);
    }
}

fn visit_records(records: &[u8], visit: &mut impl FnMut(u64, &CStr)) {
    let mut position = 0;
    while position + DIRENT_NAME_OFFSET <= records.len() {
        let reclen_bytes = [
            records[position + DIRENT_RECLEN_OFFSET],
            records[position + DIRENT_RECLEN_OFFSET + 1],
        ];
        let record_len = usize::from(u16::from_ne_bytes(reclen_bytes));
        let end = (position + record_len).min(records.len());
        if record_len == 0 {
            return;
        }
        if let Ok(name) = CStr::from_bytes_until_nul(&records[position + DIRENT_NAME_OFFSET..end]) {
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                visit(read_ino(&records[position..end]), name);
            }
        }
        position += record_len;
    }
}

fn read_ino(record: &[u8]) -> u64 {
    let mut ino_bytes = [0; 8];
    ino_bytes.copy_from_slice(&record[DIRENT_INO_OFFSET..DIRENT_INO_OFFSET + 8]);
    u64::from_ne_bytes(ino_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ino: u64, name: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; DIRENT_NAME_OFFSET];
        bytes[..8].copy_from_slice(&ino.to_ne_bytes());
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.resize(bytes.len().next_multiple_of(8), 0);
        let record_len = u16::try_from(bytes.len()).unwrap();
        bytes[DIRENT_RECLEN_OFFSET..DIRENT_RECLEN_OFFSET + 2]
            .copy_from_slice(&record_len.to_ne_bytes());
        bytes
    }

    #[test]
    fn visits_inode_and_name_skipping_dot_entries() {
        let records = [record(2, b"."), record(1, b".."), record(42, b"file.txt")].concat();
        let mut visited = Vec::new();
        visit_records(&records, &mut |ino, name: &CStr| {
            visited.push((ino, name.to_bytes().to_vec()));
        });
        assert_eq!(visited, [(42, b"file.txt".to_vec())]);
    }
}
