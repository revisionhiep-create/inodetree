use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;

use inodetree::arena::{Arena, NodeKind, ROOT};
use inodetree::scan::{ScanConfig, ScanResult, vfs};

fn scan_path(path: &Path) -> ScanResult {
    vfs::scan(&ScanConfig::new(path)).unwrap()
}

fn find(arena: &Arena, name: &str) -> Option<u32> {
    (0..arena.nodes.len() as u32).find(|&id| arena.name(id) == name)
}

#[test]
fn totals_match_known_file_sizes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/one.txt"), vec![0_u8; 1000]).unwrap();
    fs::write(dir.path().join("a/b/two.TXT"), vec![0_u8; 2000]).unwrap();

    let arena = scan_path(dir.path()).arena;
    let root = &arena.nodes[ROOT as usize];
    assert_eq!(root.file_count, 2);
    assert_eq!(root.dir_count, 2);
    let sub = &arena.nodes[find(&arena, "a").unwrap() as usize];
    assert_eq!(sub.file_count, 2);
    // Own directory sizes are included, so the root is at least the file payload
    assert!(root.logical_size >= 3000);
    assert_eq!(
        arena
            .extensions
            .name(arena.nodes[find(&arena, "two.TXT").unwrap() as usize].ext_id),
        "txt"
    );
}

#[test]
fn symlink_to_ancestor_is_a_leaf() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    let link = dir.path().join("sub").join("loop");
    #[cfg(unix)]
    symlink(dir.path(), &link).unwrap();
    #[cfg(windows)]
    match std::os::windows::fs::symlink_dir(dir.path(), &link) {
        Ok(()) => {}
        Err(err) if err.raw_os_error() == Some(1314) => return,
        Err(err) => panic!("failed to create directory symlink: {err}"),
    }

    let arena = scan_path(dir.path()).arena;
    let link = find(&arena, "loop").unwrap();
    assert_eq!(arena.nodes[link as usize].kind, NodeKind::Symlink);
    assert_eq!(
        arena.nodes[link as usize].first_child,
        inodetree::arena::NO_NODE
    );
}

#[cfg(windows)]
#[test]
fn junction_to_ancestor_is_not_descended() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    let link = dir.path().join("sub").join("loop");
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let arena = scan_path(dir.path()).arena;
    let node = find(&arena, "loop").unwrap();
    assert_eq!(arena.nodes[node as usize].kind, NodeKind::Directory);
    assert_eq!(
        arena.nodes[node as usize].first_child,
        inodetree::arena::NO_NODE
    );
}

#[test]
fn hardlinked_data_is_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("original"), vec![1_u8; 8192]).unwrap();
    fs::hard_link(dir.path().join("original"), dir.path().join("alias")).unwrap();

    let arena = scan_path(dir.path()).arena;
    let sizes: Vec<u64> = arena
        .children(ROOT)
        .map(|id| arena.nodes[id as usize].logical_size)
        .collect();
    assert_eq!(sizes.iter().filter(|&&size| size == 8192).count(), 1);
    assert_eq!(sizes.iter().filter(|&&size| size == 0).count(), 1);
    let duplicates = arena
        .nodes
        .iter()
        .filter(|node| node.is_hardlink_duplicate)
        .count();
    assert_eq!(duplicates, 1);
}

#[cfg(unix)]
#[test]
fn unreadable_directory_is_counted_not_fatal() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root bypasses permission checks
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let config = ScanConfig::new(dir.path());
    let result = vfs::scan(&config).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(config.progress.unreadable_dirs.load(Relaxed), 1);
    assert_eq!(result.arena.nodes[ROOT as usize].dir_count, 1);
}

#[cfg(unix)]
#[test]
fn does_not_descend_into_other_filesystems() {
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let marker = shm.join(format!("inodetree-test-{}", std::process::id()));
    fs::write(&marker, b"x").unwrap();
    let arena = scan_path(Path::new("/dev")).arena;
    fs::remove_file(&marker).unwrap();

    let shm_node = find(&arena, "shm").unwrap();
    assert_eq!(
        arena.nodes[shm_node as usize].first_child,
        inodetree::arena::NO_NODE
    );
}

#[test]
fn cancelled_scan_returns_a_consistent_arena() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    let config = ScanConfig::new(dir.path());
    config.cancel.store(true, Relaxed);

    let result = vfs::scan(&config).unwrap();
    assert!(result.was_cancelled);
    assert_eq!(result.arena.nodes.len(), 1);
}

#[test]
fn scanning_a_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("plain");
    fs::write(&file, b"x").unwrap();
    assert!(vfs::scan(&ScanConfig::new(&file)).is_err());
}
