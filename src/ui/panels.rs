//! Panel rendering for `App`: toolbar, status bar, extension table, tree and confirmation modal.

use std::sync::atomic::Ordering::Relaxed;

use eframe::egui::{self, Color32};

use super::app::App;
use super::background::dedupe_delete_targets;
use super::context_menu::RowAction;
use super::ext_view::{self, ExtView};
use super::session::Session;
use super::toolbar::{self, ToolbarEvent, ToolbarState};
use super::tree_view::{self, TreeView};
use super::treemap::{self, TreemapEvent, TreemapView};
use crate::actions::DeleteMode;
use crate::arena::{NodeId, ROOT};
use crate::format::{format_count, format_size};
use crate::mounts;

const WARNING_COLOR: Color32 = Color32::from_rgb(230, 170, 40);

impl App {
    pub(super) fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        let was_logical = self.use_logical;
        let active_path = self
            .session
            .as_ref()
            .map_or(self.path_input.as_str(), |session| session.arena.name(ROOT));
        let active_mount = mounts::mount_for_path(&self.mounts, active_path);
        let mut state = ToolbarState {
            path_input: &mut self.path_input,
            filter_input: &mut self.filter_input,
            match_count: self.session.as_ref().and_then(Session::match_count),
            mounts: &self.mounts,
            active_mount,
            use_logical: &mut self.use_logical,
            is_scanning: self.job.is_some(),
        };
        let event = toolbar::show(ui, &mut state);
        if self.use_logical != was_logical {
            if let Some(session) = &mut self.session {
                session.apply_size_mode(self.use_logical);
            }
        }
        match event {
            Some(ToolbarEvent::StartScan) => self.start_scan(),
            Some(ToolbarEvent::Stop) => {
                if let Some(job) = &self.job {
                    job.cancel();
                }
            }
            None => {}
        }
    }

    pub(super) fn show_status(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(job) = &self.job {
                let progress = &job.config.progress;
                ui.spinner();
                ui.label(format!(
                    "Scanning... {} files, {} folders, {}",
                    format_count(progress.files.load(Relaxed)),
                    format_count(progress.dirs.load(Relaxed)),
                    format_size(progress.allocated_bytes.load(Relaxed)),
                ));
            } else if let Some(session) = &self.session {
                let root = session.root();
                ui.label(format!(
                    "Scanned {} files in {:.2?} | {} allocated",
                    format_count(u64::from(root.file_count)),
                    session.elapsed,
                    format_size(root.allocated_size),
                ));
                if session.unreadable_dirs > 0 {
                    let text = format!(
                        "⚠ {} folders unreadable",
                        format_count(session.unreadable_dirs)
                    );
                    ui.colored_label(WARNING_COLOR, text);
                }
                if session.tree.selected.len() > 1 {
                    let deduped =
                        dedupe_delete_targets(&session.arena, &session.tree.selected_in_order());
                    let total_bytes: u64 = deduped
                        .iter()
                        .map(|&id| session.arena.nodes[id as usize].size(self.use_logical))
                        .sum();
                    ui.label(format!(
                        "| {} selected ({})",
                        format_count(session.tree.selected.len() as u64),
                        format_size(total_bytes),
                    ));
                }
            }
            if let Some(message) = self.error.clone() {
                ui.colored_label(Color32::LIGHT_RED, message);
                if ui.small_button("Dismiss").clicked() {
                    self.error = None;
                }
            }
        });
    }

    pub(super) fn show_extensions(&mut self, ui: &mut egui::Ui) {
        let Some(session) = &mut self.session else {
            return;
        };
        let view = ExtView {
            stats: &session.ext_stats,
            extensions: &session.arena.extensions,
            colors: &session.colors,
            scan_total_bytes: session.root().allocated_size,
            selected: session.selected_ext,
        };
        if let Some(ext_id) = ext_view::show(ui, &view) {
            session.toggle_extension(ext_id);
        }
    }

    pub(super) fn show_tree(&mut self, ui: &mut egui::Ui) -> Option<RowAction> {
        let Some(session) = &mut self.session else {
            ui.centered_and_justified(|ui| {
                ui.label("Pick a drive or enter a path, then press Scan")
            });
            return None;
        };
        let view = TreeView {
            arena: &session.arena,
            colors: &session.colors,
            use_logical: self.use_logical,
            highlighted_ext: session.selected_ext,
        };
        tree_view::show(ui, &mut session.tree, &view)
    }

    pub(super) fn show_treemap(&mut self, ui: &mut egui::Ui) -> Option<TreemapEvent> {
        let session = self.session.as_mut()?;
        let view = TreemapView {
            arena: &session.arena,
            colors: &session.colors,
            use_logical: self.use_logical,
            generation: session.generation,
            highlighted_ext: session.selected_ext,
            filter: session.tree.filter.as_ref(),
            selected: &session.tree.selected,
        };
        treemap::show(ui, &mut session.treemap, &view)
    }

    pub(super) fn show_delete_confirmation(&mut self, ctx: &egui::Context) {
        if self.confirming_delete.is_empty() {
            return;
        }
        let Some(session) = &self.session else {
            self.confirming_delete.clear();
            return;
        };
        let targets: Vec<NodeId> = self
            .confirming_delete
            .iter()
            .copied()
            .filter(|&id| {
                session
                    .arena
                    .nodes
                    .get(id as usize)
                    .is_some_and(|node| !node.is_deleted)
            })
            .collect();
        if targets.is_empty() {
            self.confirming_delete.clear();
            return;
        }
        let total_bytes: u64 = targets
            .iter()
            .map(|&id| session.arena.nodes[id as usize].size(self.use_logical))
            .sum();
        let is_multiple = targets.len() > 1;
        let heading = if is_multiple {
            format!("Delete {} items permanently?", targets.len())
        } else {
            String::from("Delete permanently?")
        };
        let warning = if is_multiple {
            format!(
                "You are about to permanently delete {} items ({} total):",
                targets.len(),
                format_size(total_bytes),
            )
        } else {
            format!(
                "You are about to permanently delete this item ({}):",
                format_size(total_bytes),
            )
        };
        let delete_button = if is_multiple { "Delete All" } else { "Delete" };
        let mut confirmed = None;
        let modal = egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
            ui.heading(&heading);
            ui.label(&warning);
            for &id in targets.iter().take(10) {
                ui.label(session.arena.path_of(id));
            }
            if targets.len() > 10 {
                ui.label(format!("... and {} more", targets.len() - 10));
            }
            ui.label("This cannot be undone.");
            ui.horizontal(|ui| {
                if ui.button(delete_button).clicked() {
                    confirmed = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    confirmed = Some(false);
                }
            });
        });
        match confirmed.or(modal.should_close().then_some(false)) {
            Some(true) => {
                self.confirming_delete.clear();
                self.start_delete_many(&targets, DeleteMode::Permanent);
            }
            Some(false) => self.confirming_delete.clear(),
            None => {}
        }
    }
}
