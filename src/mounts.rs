//! Discovery of real local filesystems (`/proc/self/mountinfo` on Linux, logical drives on Windows).

#[cfg(unix)]
use std::ffi::CString;
use std::io;

const REAL_FILESYSTEMS: &[&str] = &[
    "ext4", "btrfs", "xfs", "ntfs", "ntfs3", "vfat", "exfat", "zfs", "f2fs", "bcachefs", "hfsplus",
    "cifs", "smb3", "nfs", "nfs4",
];

// FUSE drivers that front real storage, unlike virtual ones such as gvfsd-fuse or portals
const REAL_FUSE_FILESYSTEMS: &[&str] = &["fuse.mergerfs", "fuse.sshfs"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub device: String,
    pub mount_point: String,
    pub filesystem: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Lists mounted real filesystems with their capacity.
#[cfg(unix)]
pub fn list_mounts() -> io::Result<Vec<MountInfo>> {
    let content = std::fs::read_to_string("/proc/self/mountinfo")?;
    let mut mounts = parse_mountinfo(&content);
    for mount in &mut mounts {
        // A mount that cannot be stat'ed (permissions, stale network share) still gets listed
        if let Ok((total, free)) = filesystem_space(&mount.mount_point) {
            mount.total_bytes = total;
            mount.free_bytes = free;
        }
    }
    Ok(mounts)
}

/// Lists mounted Windows drives with their volume label, filesystem, and capacity.
#[cfg(windows)]
pub fn list_mounts() -> io::Result<Vec<MountInfo>> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };

    // SAFETY: `GetLogicalDrives` takes no pointers and returns a 26-bit drive mask.
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut mounts = Vec::new();
    for bit in 0..26_u8 {
        if (mask & (1_u32 << bit)) == 0 {
            continue;
        }
        let letter = char::from(b'A' + bit);
        let mount_point = format!("{letter}:\\");
        let root_wide: Vec<u16> = mount_point.encode_utf16().chain(Some(0)).collect();

        // SAFETY: `root_wide` is NUL-terminated UTF-16.
        let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
        let is_supported = matches!(
            drive_type,
            DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_REMOTE | DRIVE_CDROM | DRIVE_RAMDISK
        );
        if !is_supported {
            continue;
        }

        let space = windows_drive_space(&root_wide);
        // Empty card readers and optical drives fail capacity queries; skip them so the picker stays clean.
        if matches!(drive_type, DRIVE_REMOVABLE | DRIVE_CDROM) && space.is_none() {
            continue;
        }
        let (total_bytes, free_bytes) = space.unwrap_or((0, 0));
        let (label, filesystem) = windows_volume_info(&root_wide);
        let fallback = match drive_type {
            DRIVE_REMOVABLE => "Removable Disk",
            DRIVE_REMOTE => "Network Drive",
            DRIVE_CDROM => "Optical Drive",
            DRIVE_RAMDISK => "RAM Disk",
            _ => "Local Disk",
        };
        let base_label = if label.is_empty() { fallback } else { &label };
        let device = if filesystem.is_empty() {
            base_label.to_owned()
        } else {
            format!("{base_label} ({filesystem})")
        };

        mounts.push(MountInfo {
            device,
            mount_point,
            filesystem,
            total_bytes,
            free_bytes,
        });
    }
    Ok(mounts)
}

#[cfg(windows)]
fn windows_drive_space(root_wide: &[u16]) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut free_to_caller = 0_u64;
    let mut total_bytes = 0_u64;
    // SAFETY: `root_wide` is NUL-terminated and both output pointers are valid writable `u64`s.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            root_wide.as_ptr(),
            &raw mut free_to_caller,
            &raw mut total_bytes,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some((total_bytes, free_to_caller))
}

#[cfg(windows)]
fn windows_volume_info(root_wide: &[u16]) -> (String, String) {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    let mut label_buf = [0_u16; 261];
    let mut fs_buf = [0_u16; 261];
    // SAFETY: `root_wide` is NUL-terminated and both buffers match their passed lengths.
    let ok = unsafe {
        GetVolumeInformationW(
            root_wide.as_ptr(),
            label_buf.as_mut_ptr(),
            label_buf.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_buf.as_mut_ptr(),
            fs_buf.len() as u32,
        )
    };
    if ok == 0 {
        return (String::new(), String::new());
    }
    (utf16_z_to_string(&label_buf), utf16_z_to_string(&fs_buf))
}

#[cfg(windows)]
fn utf16_z_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Pure parser so it can be tested against fixed text.
#[must_use]
pub fn parse_mountinfo(content: &str) -> Vec<MountInfo> {
    content.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<MountInfo> {
    // Fields before " - " are variable-length, so the filesystem part is located by the separator
    let (before, after) = line.split_once(" - ")?;
    let mount_point = before.split(' ').nth(4)?;
    let mut after_fields = after.split(' ');
    let filesystem = after_fields.next()?;
    let device = after_fields.next()?;
    if !is_real_filesystem(filesystem, device) {
        return None;
    }
    Some(MountInfo {
        device: decode_octal_escapes(device),
        mount_point: decode_octal_escapes(mount_point),
        filesystem: filesystem.to_owned(),
        total_bytes: 0,
        free_bytes: 0,
    })
}

fn is_real_filesystem(filesystem: &str, device: &str) -> bool {
    // ntfs-3g and exfat-fuse mount as fuseblk, which is real storage only on a block device
    REAL_FILESYSTEMS.contains(&filesystem)
        || REAL_FUSE_FILESYSTEMS.contains(&filesystem)
        || (filesystem == "fuseblk" && device.starts_with("/dev/"))
}

/// The kernel escapes space, tab, newline and backslash as `\NNN` octal sequences.
fn decode_octal_escapes(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = bytes.get(index + 1..index + 4).and_then(|digits| {
            let text = std::str::from_utf8(digits).ok()?;
            u8::from_str_radix(text, 8).ok()
        });
        match (bytes[index], octal) {
            (b'\\', Some(value)) => {
                decoded.push(value);
                index += 4;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(unix)]
fn filesystem_space(path: &str) -> io::Result<(u64, u64)> {
    let c_path = CString::new(path).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_path` is NUL-terminated and `stats` is a valid out-pointer for one `statvfs`
    let result = unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statvfs` returned 0, so it fully initialized the struct
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize;
    Ok((
        stats.f_blocks.saturating_mul(block_size),
        stats.f_bavail.saturating_mul(block_size),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_lists_at_least_one_drive() {
        let mounts = list_mounts().unwrap();
        assert_ne!(mounts.len(), 0);
        assert!(
            mounts
                .iter()
                .all(|mount| mount.mount_point.ends_with(":\\") && mount.mount_point.len() == 3)
        );
    }

    const SAMPLE: &str = "\
24 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
25 24 0:5 / /proc rw,nosuid shared:2 - proc proc rw
26 24 0:21 / /run rw shared:3 - tmpfs tmpfs rw
27 24 259:3 / /mnt/My\\040Disk rw master:4 - ntfs3 /dev/sda1 rw
28 24 0:30 / /snap/core rw - squashfs /dev/loop0 ro
29 24 0:40 / /mnt/windows rw - fuseblk /dev/sdb1 rw
30 24 0:41 / /run/user/1000/gvfs rw - fuse.gvfsd-fuse gvfsd-fuse rw";

    #[test]
    fn keeps_only_real_filesystems() {
        let mounts = parse_mountinfo(SAMPLE);
        let points: Vec<_> = mounts
            .iter()
            .map(|mount| mount.mount_point.as_str())
            .collect();
        assert_eq!(points, ["/", "/mnt/My Disk", "/mnt/windows"]);
    }

    #[test]
    fn accepts_fuseblk_only_on_block_devices() {
        assert!(is_real_filesystem("fuseblk", "/dev/sdb1"));
        assert!(!is_real_filesystem("fuseblk", "some-image"));
        assert!(!is_real_filesystem("fuse.gvfsd-fuse", "gvfsd-fuse"));
    }

    #[test]
    fn extracts_device_and_filesystem() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(mounts[0].device, "/dev/nvme0n1p2");
        assert_eq!(mounts[1].filesystem, "ntfs3");
    }
}
