//! Right-click menu shared by the tree table and the treemap.

use eframe::egui::{self, Ui};

use crate::arena::{NodeId, ROOT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    OpenInFileManager(NodeId),
    CopyPath(NodeId),
    MoveToTrash(NodeId),
    DeletePermanently(NodeId),
}

pub fn show(ui: &mut Ui, id: NodeId, selected_count: usize) -> Option<RowAction> {
    let is_root = id == ROOT;
    let can_delete = selected_count > 1 || !is_root;
    let mut chosen = None;
    let open_label = if cfg!(windows) {
        "Open in Explorer"
    } else {
        "Open in File Manager"
    };
    let copy_label = if selected_count > 1 {
        format!("Copy Full Paths ({selected_count} items)")
    } else {
        String::from("Copy Full Path")
    };
    let trash_label = match (cfg!(windows), selected_count > 1) {
        (true, true) => format!("Move to Recycle Bin ({selected_count} items)"),
        (true, false) => String::from("Move to Recycle Bin"),
        (false, true) => format!("Move to Trash ({selected_count} items)"),
        (false, false) => String::from("Move to Trash"),
    };
    let delete_label = if selected_count > 1 {
        format!("Delete Permanently ({selected_count} items)")
    } else {
        String::from("Delete Permanently")
    };

    if ui.button(open_label).clicked() {
        chosen = Some(RowAction::OpenInFileManager(id));
    }
    if ui.button(copy_label).clicked() {
        chosen = Some(RowAction::CopyPath(id));
    }
    if ui
        .add_enabled(can_delete, egui::Button::new(trash_label))
        .clicked()
    {
        chosen = Some(RowAction::MoveToTrash(id));
    }
    if ui
        .add_enabled(can_delete, egui::Button::new(delete_label))
        .clicked()
    {
        chosen = Some(RowAction::DeletePermanently(id));
    }
    if chosen.is_some() {
        ui.close();
    }
    chosen
}
