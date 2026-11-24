//! Background work of `App`: mounts listing, scans and deletions, and their result polling.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::time::Duration;

use super::app::App;
use super::session::Session;
use crate::actions::{self, DeleteMode};
use crate::arena::{Arena, NO_NODE, NodeId, ROOT};
use crate::mounts;
use crate::scan::ScanConfig;
use crate::scan::job::ScanJob;

const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

impl App {
    pub(super) fn load_mounts(&mut self) {
        let sender = self.mounts_sender.clone();
        let repaint = self.context.clone();
        let spawned = thread::Builder::new()
            .name("inodetree-mounts".into())
            .spawn(move || {
                // An unreadable mountinfo only leaves the drive picker empty
                let _ = sender.send(mounts::list_mounts().unwrap_or_default());
                repaint.request_repaint();
            });
        if let Err(error) = spawned {
            self.error = Some(format!("Cannot list drives: {error}"));
        }
    }

    pub(super) fn start_scan(&mut self) {
        let path = self.path_input.trim().to_owned();
        if path.is_empty() {
            self.error = Some("Enter a path or pick a drive first".into());
            return;
        }
        if let Some(previous) = self.job.take() {
            previous.cancel();
        }
        let repaint = self.context.clone();
        match ScanJob::start(ScanConfig::new(path), move || repaint.request_repaint()) {
            Ok(job) => {
                self.job = Some(job);
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Cannot start scan: {error}")),
        }
    }

    pub(super) fn poll_mounts(&mut self) {
        if let Ok(mounts) = self.mounts_receiver.try_recv() {
            self.mounts = mounts;
        }
    }

    pub(super) fn poll_scan(&mut self) {
        let Some(job) = &self.job else { return };
        let Some(result) = job.try_take() else {
            self.context.request_repaint_after(PROGRESS_POLL_INTERVAL);
            return;
        };
        let unreadable_dirs = job.config.progress.unreadable_dirs.load(Relaxed);
        self.job = None;
        match result {
            Ok(result) if !result.was_cancelled => {
                let mut session = Session::new(result, unreadable_dirs);
                if self.use_logical {
                    session.apply_size_mode(true);
                }
                self.session = Some(session);
                self.pending_deletes.clear();
                self.confirming_delete.clear();
                self.load_mounts();
            }
            // A cancelled scan keeps the previous tree on screen
            Ok(_) => {}
            Err(error) => self.error = Some(format!("Scan failed: {error}")),
        }
    }

    pub(super) fn poll_actions(&mut self) {
        let mut any_deleted = false;
        while let Ok(result) = self.action_receiver.try_recv() {
            let was_pending = self.pending_deletes.remove(&result.id);
            match (result.outcome, &mut self.session) {
                (Ok(()), Some(session)) if was_pending => {
                    session.arena.mark_deleted(result.id);
                    any_deleted = true;
                }
                (Err(message), _) => self.error = Some(format!("Delete failed: {message}")),
                _ => {}
            }
        }
        if any_deleted {
            if let Some(session) = &mut self.session {
                session.refresh_after_delete();
            }
            self.load_mounts();
        }
    }

    pub(super) fn open_in_file_manager(&mut self, id: NodeId) {
        let Some(session) = &self.session else { return };
        let path = session.arena.path_of(id);
        if let Err(error) = actions::open_in_file_manager(Path::new(&path)) {
            self.error = Some(format!("Cannot open {path}: {error}"));
        }
    }

    pub(super) fn start_delete_many(&mut self, ids: &[NodeId], mode: DeleteMode) {
        for &id in ids {
            self.start_delete(id, mode);
        }
    }

    pub(super) fn start_delete(&mut self, id: NodeId, mode: DeleteMode) {
        let Some(session) = &self.session else { return };
        let path = match actions::target_path(&session.arena, id) {
            Ok(path) => path,
            Err(error) => {
                self.error = Some(format!("Cannot delete: {error}"));
                return;
            }
        };
        if !self.pending_deletes.insert(id) {
            return;
        }
        let repaint = self.context.clone();
        let spawned =
            actions::spawn_delete(id, path, mode, self.action_sender.clone(), move || {
                repaint.request_repaint();
            });
        if let Err(error) = spawned {
            self.pending_deletes.remove(&id);
            self.error = Some(format!("Cannot start delete: {error}"));
        }
    }
}

/// Drops nodes already deleted, excludes `ROOT` when other nodes are selected, and removes any
/// node whose ancestor is also in `ids` so parent+child selections do not race during deletion.
pub(super) fn dedupe_delete_targets(arena: &Arena, ids: &[NodeId]) -> Vec<NodeId> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for &id in ids {
        if let Some(node) = arena.nodes.get(id as usize) {
            if !node.is_deleted && seen.insert(id) {
                unique.push(id);
            }
        }
    }
    if unique.len() > 1 {
        unique.retain(|&id| id != ROOT);
    }
    let selected: HashSet<NodeId> = unique.iter().copied().collect();
    unique
        .into_iter()
        .filter(|&id| {
            let mut ancestor = arena.nodes[id as usize].parent;
            while ancestor != NO_NODE {
                if selected.contains(&ancestor) {
                    return false;
                }
                ancestor = arena.nodes[ancestor as usize].parent;
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::dedupe_delete_targets;
    use crate::arena::{Arena, ChildBatch, NewNode, NodeId, NodeKind, ROOT};

    fn add(arena: &mut Arena, parent: NodeId, entries: &[(&str, NodeKind, u64)]) -> NodeId {
        let mut batch = ChildBatch::default();
        for &(name, kind, size) in entries {
            let name_start = batch.names.len();
            batch.names.push_str(name);
            batch.entries.push(NewNode {
                name_start,
                name_end: batch.names.len(),
                kind,
                logical_size: size,
                allocated_size: size,
                mtime_secs: 0,
                is_hardlink_duplicate: false,
            });
        }
        arena.append_children(parent, &batch)
    }

    #[test]
    fn dedupe_drops_descendants_and_root_when_others_are_selected() {
        let mut arena = Arena::with_root("/r", 0, 0, 0);
        let first = add(
            &mut arena,
            ROOT,
            &[
                ("dir", NodeKind::Directory, 0),
                ("other", NodeKind::File, 10),
            ],
        );
        let child = add(&mut arena, first, &[("nested", NodeKind::File, 5)]);
        let other = first + 1;

        assert_eq!(
            dedupe_delete_targets(&arena, &[ROOT, first, child, other, child]),
            [first, other]
        );
        assert_eq!(dedupe_delete_targets(&arena, &[ROOT]), [ROOT]);
    }
}
