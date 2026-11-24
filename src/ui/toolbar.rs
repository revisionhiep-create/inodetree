//! Top bar: drive selector, path input, scan control, size mode, and disk space bar.

use eframe::egui::{self, Color32, Key, Rect, Sense, StrokeKind, Ui, Vec2};

use crate::format::{format_count, format_size};
use crate::mounts::MountInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarEvent {
    StartScan,
    Stop,
}

/// Widget id of the search box, so `Ctrl+F` can focus it from outside the toolbar.
pub const FILTER_FIELD_ID: &str = "filter_input";

const DISK_BAR_SIZE: Vec2 = Vec2::new(100.0, 14.0);
const DISK_TRACK_COLOR: Color32 = Color32::from_rgb(42, 46, 56);
const DISK_BORDER_COLOR: Color32 = Color32::from_rgb(72, 78, 92);
const DISK_OK_COLOR: Color32 = Color32::from_rgb(56, 138, 224);
const DISK_WARN_COLOR: Color32 = Color32::from_rgb(230, 165, 45);
const DISK_FULL_COLOR: Color32 = Color32::from_rgb(220, 76, 65);

pub struct ToolbarState<'a> {
    pub path_input: &'a mut String,
    pub filter_input: &'a mut String,
    pub match_count: Option<usize>,
    pub mounts: &'a [MountInfo],
    pub active_mount: Option<&'a MountInfo>,
    pub use_logical: &'a mut bool,
    pub is_scanning: bool,
}

pub fn show(ui: &mut Ui, state: &mut ToolbarState<'_>) -> Option<ToolbarEvent> {
    let mut event = None;
    ui.horizontal_wrapped(|ui| {
        if drive_picker(ui, state.mounts, state.path_input) {
            event = Some(ToolbarEvent::StartScan);
        }
        let path_field = ui.add(
            egui::TextEdit::singleline(state.path_input)
                .hint_text("Path to scan")
                .desired_width(240.0),
        );
        let submitted = path_field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
        if state.is_scanning {
            if ui.button("Stop").clicked() {
                event = Some(ToolbarEvent::Stop);
            }
        } else if ui.button("Scan").clicked() || submitted {
            event = Some(ToolbarEvent::StartScan);
        }
        ui.separator();
        filter_field(ui, state.filter_input, state.match_count);
        ui.separator();
        ui.selectable_value(state.use_logical, false, "Allocated size");
        ui.selectable_value(state.use_logical, true, "Logical size");
        if let Some(mount) = state.active_mount {
            ui.separator();
            disk_space_bar(ui, mount);
        }
    });
    event
}

fn filter_field(ui: &mut Ui, filter_input: &mut String, match_count: Option<usize>) {
    let field = ui.add(
        egui::TextEdit::singleline(filter_input)
            .id(egui::Id::new(FILTER_FIELD_ID))
            .hint_text("Filter: name or *.ext (Ctrl+F)")
            .desired_width(170.0),
    );
    let escape_pressed = ui.input(|input| input.key_pressed(Key::Escape));
    if (field.has_focus() || field.lost_focus()) && escape_pressed {
        filter_input.clear();
        field.surrender_focus();
    }
    if let Some(count) = match_count {
        ui.label(format!("{} matches", format_count(count as u64)));
    }
}

fn disk_space_bar(ui: &mut Ui, mount: &MountInfo) {
    let used = mount.total_bytes.saturating_sub(mount.free_bytes);
    let fraction = if mount.total_bytes == 0 {
        0.0
    } else {
        (used as f64 / mount.total_bytes as f64).clamp(0.0, 1.0) as f32
    };
    let percent = fraction * 100.0;
    let fill_color = if fraction >= 0.90 {
        DISK_FULL_COLOR
    } else if fraction >= 0.75 {
        DISK_WARN_COLOR
    } else {
        DISK_OK_COLOR
    };

    let (rect, response) = ui.allocate_exact_size(DISK_BAR_SIZE, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, DISK_TRACK_COLOR);
    if fraction > 0.0 {
        let filled = Rect::from_min_size(
            rect.min,
            Vec2::new((rect.width() * fraction).max(2.0), rect.height()),
        );
        painter.rect_filled(filled, 3.0, fill_color);
    }
    painter.rect_stroke(rect, 3.0, (1.0, DISK_BORDER_COLOR), StrokeKind::Inside);

    response.on_hover_ui(|ui| {
        ui.strong(&mount.mount_point);
        if !mount.filesystem.is_empty() {
            ui.label(format!("Filesystem: {}", mount.filesystem));
        }
        ui.label(format!("Used: {} ({percent:.1}%)", format_size(used)));
        ui.label(format!(
            "Free: {} ({:.1}%)",
            format_size(mount.free_bytes),
            100.0 - percent,
        ));
        ui.label(format!("Total: {}", format_size(mount.total_bytes)));
    });

    ui.label(format!(
        "{} used / {} ({} free)",
        format_size(used),
        format_size(mount.total_bytes),
        format_size(mount.free_bytes),
    ));
}

/// Returns `true` when the user picked a mount, whose path is written to `path_input`.
fn drive_picker(ui: &mut Ui, mounts: &[MountInfo], path_input: &mut String) -> bool {
    let mut picked = false;
    egui::ComboBox::from_id_salt("drive_picker")
        .selected_text("Select drive")
        .width(360.0)
        .show_ui(ui, |ui| {
            for mount in mounts {
                if ui.selectable_label(false, mount_label(mount)).clicked() {
                    path_input.clone_from(&mount.mount_point);
                    picked = true;
                }
            }
        });
    picked
}

fn mount_label(mount: &MountInfo) -> String {
    let used = mount.total_bytes.saturating_sub(mount.free_bytes);
    format!(
        "{} - {} - {} / {}",
        mount.mount_point,
        mount.device,
        format_size(used),
        format_size(mount.total_bytes)
    )
}
