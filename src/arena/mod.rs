//! Flat node arena: one `Vec<Node>` plus one contiguous name pool.

mod build;
mod mutate;

use crate::extensions::ExtensionTable;

pub use build::{ChildBatch, NewNode, StringPool};

pub type NodeId = u32;
pub const NO_NODE: NodeId = u32::MAX;
pub const ROOT: NodeId = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeKind {
    Directory = 0,
    File = 1,
    Symlink = 2,
    Other = 3,
}

/// 56 bytes with alignment (the SPEC comment says 48, but its own field list does not fit).
#[derive(Debug, Clone)]
pub struct Node {
    pub parent: NodeId,
    pub first_child: NodeId,
    pub next_sibling: NodeId,
    pub name_offset: u32,
    pub name_len: u16,
    pub ext_id: u16,
    pub kind: NodeKind,
    pub is_deleted: bool,
    /// Counted once per inode: later hard links keep a node but contribute no size.
    pub is_hardlink_duplicate: bool,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub file_count: u32,
    pub dir_count: u32,
    pub mtime_secs: i64,
}

impl Node {
    #[must_use]
    pub fn size(&self, use_logical: bool) -> u64 {
        if use_logical {
            self.logical_size
        } else {
            self.allocated_size
        }
    }
}

#[derive(Debug, Default)]
pub struct Arena {
    pub nodes: Vec<Node>,
    pub pool: StringPool,
    pub extensions: ExtensionTable,
}

impl Arena {
    /// Creates an arena whose node 0 is the scan root, named after its full path.
    #[must_use]
    pub fn with_root(path: &str, logical_size: u64, allocated_size: u64, mtime_secs: i64) -> Self {
        let mut arena = Self::default();
        let (name_offset, name_len) = arena.pool.push(path);
        arena.nodes.push(Node {
            parent: NO_NODE,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
            name_offset,
            name_len,
            ext_id: 0,
            kind: NodeKind::Directory,
            is_deleted: false,
            is_hardlink_duplicate: false,
            logical_size,
            allocated_size,
            file_count: 0,
            dir_count: 0,
            mtime_secs,
        });
        arena
    }

    /// Inserts `batch` as the children of `parent` and returns the first new id.
    /// Ids are contiguous, so the child at batch index `i` has id `first + i`.
    pub fn append_children(&mut self, parent: NodeId, batch: &ChildBatch) -> NodeId {
        let first = self.nodes.len() as NodeId;
        let count = batch.entries.len();
        if count == 0 {
            return NO_NODE;
        }
        self.nodes.reserve(count);
        for (index, entry) in batch.entries.iter().enumerate() {
            let name = &batch.names[entry.name_start..entry.name_end];
            let (name_offset, name_len) = self.pool.push(name);
            let counts_as_file = matches!(entry.kind, NodeKind::File | NodeKind::Symlink);
            let ext_id = if counts_as_file {
                self.extensions.intern(name)
            } else {
                0
            };
            self.nodes.push(Node {
                parent,
                first_child: NO_NODE,
                next_sibling: if index + 1 < count {
                    first + index as NodeId + 1
                } else {
                    NO_NODE
                },
                name_offset,
                name_len,
                ext_id,
                kind: entry.kind,
                is_deleted: false,
                is_hardlink_duplicate: entry.is_hardlink_duplicate,
                logical_size: entry.logical_size,
                allocated_size: entry.allocated_size,
                file_count: u32::from(counts_as_file),
                dir_count: 0,
                mtime_secs: entry.mtime_secs,
            });
        }
        self.nodes[parent as usize].first_child = first;
        first
    }

    #[must_use]
    pub fn name(&self, id: NodeId) -> &str {
        let node = &self.nodes[id as usize];
        self.pool.get(node.name_offset, node.name_len)
    }

    #[must_use]
    pub fn path_of(&self, id: NodeId) -> String {
        let mut components = Vec::new();
        let mut current = id;
        while current != NO_NODE {
            components.push(self.name(current));
            current = self.nodes[current as usize].parent;
        }
        let sep = if components.last().is_some_and(|root| root.contains('\\')) {
            '\\'
        } else {
            '/'
        };
        let mut path = String::new();
        for component in components.into_iter().rev() {
            if !path.is_empty() && !path.ends_with(['/', '\\']) {
                path.push(sep);
            }
            path.push_str(component);
        }
        path
    }

    /// Chain of ids from the root down to `id`, inclusive.
    #[must_use]
    pub fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut chain = Vec::new();
        let mut current = id;
        while current != NO_NODE {
            chain.push(current);
            current = self.nodes[current as usize].parent;
        }
        chain.reverse();
        chain
    }

    /// `id` itself if alive, otherwise its closest ancestor that was not deleted.
    #[must_use]
    pub fn nearest_live(&self, id: NodeId) -> NodeId {
        self.ancestry(id)
            .into_iter()
            .rev()
            .find(|&candidate| !self.nodes[candidate as usize].is_deleted)
            .unwrap_or(ROOT)
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut next = self.nodes[id as usize].first_child;
        std::iter::from_fn(move || {
            if next == NO_NODE {
                return None;
            }
            let current = next;
            next = self.nodes[current as usize].next_sibling;
            Some(current)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_stays_within_the_size_budget() {
        assert!(std::mem::size_of::<Node>() <= 56);
    }

    #[test]
    fn path_of_preserves_windows_and_unix_separators() {
        let mut win_root = Arena::with_root("C:\\", 0, 0, 0);
        let mut batch = ChildBatch::default();
        batch.names.push_str("Users");
        batch.entries.push(NewNode {
            name_start: 0,
            name_end: 5,
            kind: NodeKind::Directory,
            logical_size: 0,
            allocated_size: 0,
            mtime_secs: 0,
            is_hardlink_duplicate: false,
        });
        let users = win_root.append_children(ROOT, &batch);
        assert_eq!(win_root.path_of(users), "C:\\Users");

        let mut win_sub = Arena::with_root("C:\\Users", 0, 0, 0);
        let file = win_sub.append_children(ROOT, &batch);
        assert_eq!(win_sub.path_of(file), "C:\\Users\\Users");

        let mut unc = Arena::with_root("\\\\server\\share", 0, 0, 0);
        let share_dir = unc.append_children(ROOT, &batch);
        assert_eq!(unc.path_of(share_dir), "\\\\server\\share\\Users");
    }
}
