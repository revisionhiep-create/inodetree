//! Cell painters for the tree table rows.

use eframe::egui::{self, Align, Color32, Label, Layout, Rect, Sense, Ui, Vec2};

use super::tree_model::Row;
use super::tree_view::{Pending, RowState, TreeView};
use crate::arena::{NO_NODE, Node, NodeKind};
use crate::format::format_size;

const INDENT_PER_LEVEL: f32 = 16.0;
const ARROW_WIDTH: f32 = 14.0;
const NAME_RIGHT_PADDING: f32 = 12.0;
const FOLDER_COLOR: Color32 = Color32::from_rgb(120, 144, 190);
const BAR_COLOR: Color32 = Color32::from_rgba_premultiplied(40, 70, 112, 63);
const HIGHLIGHT_COLOR: Color32 = Color32::from_rgba_premultiplied(120, 90, 0, 120);

pub(super) fn name_cell(
    ui: &mut Ui,
    view: &TreeView<'_>,
    entry: Row,
    state: RowState,
    pending: &mut Pending,
) {
    let node = &view.arena.nodes[entry.id as usize];
    highlight_if_matching(ui, view, node);
    let cell_width = ui.max_rect().width();
    let text = view.arena.name(entry.id);
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let text_width = ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font_id, Color32::PLACEHOLDER)
            .rect
            .width()
    });
    let needed = entry.depth as f32 * INDENT_PER_LEVEL
        + ARROW_WIDTH
        + 10.0
        + ui.spacing().item_spacing.x * 3.0
        + text_width
        + NAME_RIGHT_PADDING;
    pending.max_needed_name_width = pending.max_needed_name_width.max(needed);
    pending.current_name_width = pending.current_name_width.max(cell_width);

    ui.add_space(entry.depth as f32 * INDENT_PER_LEVEL);
    if node.kind == NodeKind::Directory && node.first_child != NO_NODE {
        // Painted instead of a text glyph: the default egui font has no down-pointing triangle
        let (_, response) = ui.allocate_exact_size(Vec2::splat(ARROW_WIDTH), Sense::click());
        let openness = if state.is_expanded { 1.0 } else { 0.0 };
        egui::collapsing_header::paint_default_icon(ui, openness, &response);
        if response.clicked() {
            pending.toggle = Some(entry.id);
        }
    } else {
        ui.add_space(ARROW_WIDTH);
    }
    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter()
        .circle_filled(dot.center(), 4.0, node_color(view, node));
    ui.add(Label::new(text).selectable(false));
}

fn node_color(view: &TreeView<'_>, node: &Node) -> Color32 {
    if node.kind == NodeKind::Directory {
        return FOLDER_COLOR;
    }
    view.colors
        .get(usize::from(node.ext_id))
        .copied()
        .unwrap_or(Color32::GRAY)
}

fn highlight_if_matching(ui: &Ui, view: &TreeView<'_>, node: &Node) {
    let is_match = node.kind == NodeKind::File && view.highlighted_ext == Some(node.ext_id);
    if is_match {
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, HIGHLIGHT_COLOR);
    }
}

pub(super) fn size_cell(ui: &mut Ui, view: &TreeView<'_>, node: &Node) {
    let size = node.size(view.use_logical);
    let cell = ui.max_rect();
    let bar_width = cell.width() * parent_fraction(view, node, size);
    let bar = Rect::from_min_size(cell.min, Vec2::new(bar_width, cell.height()));
    ui.painter().rect_filled(bar, 2.0, BAR_COLOR);
    right_aligned(ui, &format_size(size));
}

fn parent_fraction(view: &TreeView<'_>, node: &Node, size: u64) -> f32 {
    let parent_size = if node.parent == NO_NODE {
        size
    } else {
        view.arena.nodes[node.parent as usize].size(view.use_logical)
    };
    if parent_size == 0 {
        0.0
    } else {
        (size as f64 / parent_size as f64) as f32
    }
}

pub(super) fn percent_of_parent(view: &TreeView<'_>, node: &Node) -> String {
    let fraction = parent_fraction(view, node, node.size(view.use_logical));
    format!("{:.1} %", fraction * 100.0)
}

pub(super) fn right_aligned(ui: &mut Ui, text: &str) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.label(text);
    });
}
