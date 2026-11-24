//! Flattened, expandable view of the arena. Pure data, so it can be tested without a display.

use std::collections::HashSet;

use crate::arena::{Arena, NO_NODE, NodeId, NodeKind, ROOT};
use crate::filter::FilterMask;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Replace,
    Toggle,
    Range,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub id: NodeId,
    pub depth: u32,
}

#[derive(Debug, Default)]
pub struct TreeModel {
    pub expanded: HashSet<NodeId>,
    pub rows: Vec<Row>,
    pub selected: HashSet<NodeId>,
    pub anchor: Option<NodeId>,
    /// Row to bring into view on the next frame, then cleared by the table
    pub scroll_to: Option<NodeId>,
    /// Active search result: rows are limited to matches and the folders leading to them
    pub filter: Option<FilterMask>,
    // Expansion the user had before searching, restored when the search is cleared
    saved_expanded: Option<HashSet<NodeId>>,
    /// One-frame minimum Name column width bump requested when visible rows need more room
    pub pending_name_width: Option<f32>,
    /// Widest width auto-expansion has already grown to for the current navigation state
    pub auto_expanded_to: f32,
}

impl TreeModel {
    /// Starts from a fresh scan: only the root is expanded.
    pub fn reset(&mut self, arena: &Arena) {
        self.expanded.clear();
        self.expanded.insert(ROOT);
        self.selected.clear();
        self.anchor = None;
        self.pending_name_width = None;
        self.auto_expanded_to = 0.0;
        self.rebuild(arena);
    }

    /// Rebuilds the visible rows depth-first. Call after expansion or deletion changes.
    pub fn rebuild(&mut self, arena: &Arena) {
        self.rows.clear();
        let mut stack = vec![Row { id: ROOT, depth: 0 }];
        while let Some(row) = stack.pop() {
            self.rows.push(row);
            if self.expanded.contains(&row.id) {
                let children: Vec<NodeId> = arena
                    .children(row.id)
                    .filter(|&child| {
                        !arena.nodes[child as usize].is_deleted
                            && self
                                .filter
                                .as_ref()
                                .is_none_or(|mask| mask.is_visible(child))
                    })
                    .collect();
                // Pushed in reverse so the largest child is popped first
                stack.extend(children.into_iter().rev().map(|id| Row {
                    id,
                    depth: row.depth + 1,
                }));
            }
        }
        self.selected
            .retain(|&id| !arena.nodes[id as usize].is_deleted);
        if self
            .anchor
            .is_some_and(|id| arena.nodes[id as usize].is_deleted)
        {
            self.anchor = None;
        }
    }

    /// Applies or clears a search result. While a search is active every folder that leads to a
    /// match is open, so results are visible without clicking through the tree.
    pub fn set_filter(&mut self, arena: &Arena, mask: Option<FilterMask>) {
        match (&self.filter, &mask) {
            (None, Some(_)) => self.saved_expanded = Some(std::mem::take(&mut self.expanded)),
            (Some(_), None) => {
                if let Some(saved) = self.saved_expanded.take() {
                    self.expanded = saved;
                }
            }
            _ => {}
        }
        if let Some(mask) = &mask {
            self.expanded = (0..arena.nodes.len() as NodeId)
                .filter(|&id| mask.has_match_below(id))
                .collect();
            self.expanded.insert(ROOT);
        }
        self.filter = mask;
        self.auto_expanded_to = 0.0;
        self.rebuild(arena);
    }

    pub fn select(&mut self, id: NodeId, mode: SelectionMode) {
        match mode {
            SelectionMode::Replace => {
                self.selected.clear();
                self.selected.insert(id);
                self.anchor = Some(id);
            }
            SelectionMode::Toggle => {
                if self.selected.remove(&id) {
                    if self.anchor == Some(id) {
                        self.anchor = self.selected_in_order().into_iter().next();
                    }
                } else {
                    self.selected.insert(id);
                    self.anchor = Some(id);
                }
            }
            SelectionMode::Range => {
                let range = self
                    .anchor
                    .and_then(|anchor| self.row_index(anchor).zip(self.row_index(id)));
                if let Some((start, end)) = range {
                    let (lo, hi) = if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    };
                    self.selected.clear();
                    self.selected
                        .extend(self.rows[lo..=hi].iter().map(|row| row.id));
                } else {
                    self.selected.clear();
                    self.selected.insert(id);
                    self.anchor = Some(id);
                }
            }
        }
    }

    pub fn ensure_selected_for_menu(&mut self, id: NodeId) {
        if !self.selected.contains(&id) {
            self.select(id, SelectionMode::Replace);
        }
    }

    /// Selects `id`, expands its ancestors and asks the table to scroll to it.
    pub fn select_and_reveal(&mut self, arena: &Arena, id: NodeId, mode: SelectionMode) {
        let mut ancestor = arena.nodes[id as usize].parent;
        while ancestor != NO_NODE {
            self.expanded.insert(ancestor);
            ancestor = arena.nodes[ancestor as usize].parent;
        }
        self.scroll_to = Some(id);
        self.auto_expanded_to = 0.0;
        self.rebuild(arena);
        self.select(id, mode);
    }

    #[must_use]
    pub fn selected_in_order(&self) -> Vec<NodeId> {
        let mut ordered: Vec<NodeId> = self
            .rows
            .iter()
            .filter_map(|row| self.selected.contains(&row.id).then_some(row.id))
            .collect();
        if ordered.len() < self.selected.len() {
            let mut hidden: Vec<NodeId> = self
                .selected
                .iter()
                .copied()
                .filter(|id| !ordered.contains(id))
                .collect();
            hidden.sort_unstable();
            ordered.extend(hidden);
        }
        ordered
    }

    #[must_use]
    pub fn row_index(&self, id: NodeId) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    pub fn toggle(&mut self, arena: &Arena, id: NodeId) {
        if arena.nodes[id as usize].kind != NodeKind::Directory {
            return;
        }
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.auto_expanded_to = 0.0;
        self.rebuild(arena);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{ChildBatch, NewNode};

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

    /// root / { docs/ { a, b }, big }
    fn sample() -> (Arena, NodeId) {
        let mut arena = Arena::with_root("/r", 0, 0, 0);
        let first = add(
            &mut arena,
            ROOT,
            &[
                ("docs", NodeKind::Directory, 0),
                ("big", NodeKind::File, 900),
            ],
        );
        add(
            &mut arena,
            first,
            &[("a", NodeKind::File, 5), ("b", NodeKind::File, 7)],
        );
        arena.aggregate();
        (arena, first)
    }

    fn names(model: &TreeModel, arena: &Arena) -> Vec<String> {
        model
            .rows
            .iter()
            .map(|row| arena.name(row.id).to_owned())
            .collect()
    }

    #[test]
    fn reset_shows_only_the_roots_children() {
        let (arena, _) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);
        assert_eq!(names(&model, &arena), ["/r", "big", "docs"]);
    }

    #[test]
    fn toggle_expands_and_collapses_in_order() {
        let (arena, docs) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);
        model.toggle(&arena, docs);
        assert_eq!(names(&model, &arena), ["/r", "big", "docs", "b", "a"]);
        assert_eq!(model.rows[3].depth, 2);
        model.toggle(&arena, docs);
        assert_eq!(model.rows.len(), 3);
    }

    #[test]
    fn toggling_a_file_does_nothing() {
        let (arena, _) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);
        model.toggle(&arena, 2);
        assert_eq!(model.rows.len(), 3);
    }

    #[test]
    fn deleted_nodes_disappear_and_clear_the_selection() {
        let (mut arena, _) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);
        model.select(2, SelectionMode::Replace);
        arena.mark_deleted(2);
        model.rebuild(&arena);
        assert_eq!(names(&model, &arena), ["/r", "docs"]);
        assert!(model.selected.is_empty());
        assert_eq!(model.anchor, None);
    }

    #[test]
    fn select_and_reveal_expands_ancestors_and_targets_the_row() {
        let (arena, docs) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);
        model.select_and_reveal(&arena, docs + 3, SelectionMode::Replace);
        assert!(model.expanded.contains(&docs));
        assert!(model.selected.contains(&(docs + 3)));
        assert_eq!(model.scroll_to, Some(docs + 3));
        assert_eq!(model.row_index(docs + 3), Some(3));
        assert_eq!(model.row_index(999), None);
    }

    #[test]
    fn a_filter_limits_rows_opens_matches_and_restores_expansion_when_cleared() {
        use crate::filter::NameFilter;
        let (arena, docs) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);

        let mask = FilterMask::build(&arena, &NameFilter::parse("a").unwrap());
        model.set_filter(&arena, Some(mask));
        assert_eq!(names(&model, &arena), ["/r", "docs", "a"]);
        assert!(model.expanded.contains(&docs));

        model.set_filter(&arena, None);
        assert_eq!(names(&model, &arena), ["/r", "big", "docs"]);
        assert!(!model.expanded.contains(&docs));
    }

    #[test]
    fn ctrl_and_shift_selection_support_multi_select_forward_and_backward() {
        let (arena, docs) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);
        model.toggle(&arena, docs);
        // rows: ["/r" (0), "big" (2), "docs" (1), "b" (4), "a" (3)]
        let big = model.rows[1].id;
        let b_id = model.rows[3].id;
        let a_id = model.rows[4].id;

        model.select(big, SelectionMode::Replace);
        model.select(b_id, SelectionMode::Toggle);
        assert_eq!(model.selected_in_order(), [big, b_id]);

        // Toggle off
        model.select(big, SelectionMode::Toggle);
        assert_eq!(model.selected_in_order(), [b_id]);

        // Range backwards from b_id (anchor) to big
        model.select(big, SelectionMode::Range);
        assert_eq!(model.selected_in_order(), [big, docs, b_id]);
        assert_eq!(model.anchor, Some(b_id));

        // Range forwards from b_id (anchor) to a_id
        model.select(a_id, SelectionMode::Range);
        assert_eq!(model.selected_in_order(), [b_id, a_id]);
    }

    #[test]
    fn right_click_preserves_existing_multi_selection() {
        let (arena, docs) = sample();
        let mut model = TreeModel::default();
        model.reset(&arena);
        let big = model.rows[1].id;

        model.select(big, SelectionMode::Replace);
        model.select(docs, SelectionMode::Toggle);
        model.ensure_selected_for_menu(docs);
        assert_eq!(model.selected.len(), 2);

        model.ensure_selected_for_menu(ROOT);
        assert_eq!(model.selected_in_order(), [ROOT]);
    }
}
