//! Interactive treemap panel: cached layout and mesh, hover tooltip, selection and zoom.

use std::collections::HashSet;
use std::sync::Arc;

use eframe::egui::{Color32, Mesh, Rect, Sense, Shape, Stroke, StrokeKind, Ui};

use super::breadcrumb;
use super::context_menu::{self, RowAction};
use super::tree_model::SelectionMode;
use super::treemap_labels::{self, LabelSpot};
use super::treemap_mesh::{BACKGROUND, build_mesh, fill_color, to_rect};
use crate::arena::{Arena, NodeId, NodeKind};
use crate::filter::FilterMask;
use crate::format::{format_mtime, format_size};
use crate::treemap::{Bounds, TreemapRect, hit_test, layout};

const SELECTION_STROKE: Stroke = Stroke {
    width: 2.0,
    color: Color32::WHITE,
};
const HOVER_STROKE: Stroke = Stroke {
    width: 1.0,
    color: Color32::from_rgb(255, 220, 90),
};

pub struct TreemapView<'a> {
    pub arena: &'a Arena,
    pub colors: &'a [Color32],
    pub use_logical: bool,
    /// Bumped whenever the arena changes under the same scan (deletions, re-sorting)
    pub generation: u64,
    pub highlighted_ext: Option<u16>,
    /// Search box result: files outside it are dimmed, like with an extension highlight
    pub filter: Option<&'a FilterMask>,
    pub selected: &'a HashSet<NodeId>,
}

pub enum TreemapEvent {
    Select(NodeId, SelectionMode),
    SecondarySelect(NodeId),
    Action(RowAction),
}

#[derive(Default)]
pub struct TreemapState {
    zoom: NodeId,
    cache: Option<Cache>,
    // Node under the pointer when the context menu opened, so it stays stable while open
    menu_target: Option<NodeId>,
}

#[derive(PartialEq)]
struct CacheKey {
    zoom: NodeId,
    area: [i32; 4],
    use_logical: bool,
    generation: u64,
    highlighted_ext: Option<u16>,
}

struct Cache {
    key: CacheKey,
    rects: Vec<TreemapRect>,
    mesh: Arc<Mesh>,
    labels: Vec<LabelSpot>,
}

pub fn show(ui: &mut Ui, state: &mut TreemapState, view: &TreemapView<'_>) -> Option<TreemapEvent> {
    // A deletion can remove the zoomed directory, so fall back to its closest live ancestor
    state.zoom = view.arena.nearest_live(state.zoom);
    if let Some(target) = breadcrumb::show(ui, view.arena, state.zoom) {
        state.zoom = target;
    }
    let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click());
    state.refresh_cache(response.rect, view);
    let Some(cache) = &state.cache else {
        return None;
    };

    painter.rect_filled(response.rect, 0.0, BACKGROUND);
    painter.add(Shape::Mesh(Arc::clone(&cache.mesh)));
    treemap_labels::paint(&painter, response.rect, view.arena, &cache.labels);
    if !view.selected.is_empty() {
        for rect in &cache.rects {
            if view.selected.contains(&rect.id) {
                painter.rect_stroke(
                    to_rect(rect.bounds, 0.0),
                    0.0,
                    SELECTION_STROKE,
                    StrokeKind::Inside,
                );
            }
        }
    }
    let hovered = response
        .hover_pos()
        .and_then(|pointer| hit_test(&cache.rects, pointer.x, pointer.y));
    if let Some(rect) = hovered.and_then(|id| find_rect(&cache.rects, id)) {
        painter.rect_stroke(rect, 0.0, HOVER_STROKE, StrokeKind::Inside);
    }
    let mut event = None;
    if response.secondary_clicked() {
        state.menu_target = hovered;
        if let Some(id) = hovered {
            event = Some(TreemapEvent::SecondarySelect(id));
        }
    }
    if let Some(id) = hovered {
        let is_directory = view.arena.nodes[id as usize].kind == NodeKind::Directory;
        if response.double_clicked() && is_directory {
            state.zoom = id;
        } else if response.clicked() {
            let modifiers = ui.input(|input| input.modifiers);
            let mode = if modifiers.shift {
                SelectionMode::Range
            } else if modifiers.command {
                SelectionMode::Toggle
            } else {
                SelectionMode::Replace
            };
            event = Some(TreemapEvent::Select(id, mode));
        }
        response
            .clone()
            .on_hover_ui_at_pointer(|ui| tooltip(ui, view.arena, id));
    }
    // Right-clicking empty space must not open an empty menu, so only attach it to a real target
    if let Some(target) = state.menu_target {
        let effective_count = if view.selected.contains(&target) {
            view.selected.len()
        } else {
            1
        };
        response.context_menu(|ui| {
            if let Some(action) = context_menu::show(ui, target, effective_count) {
                event = Some(TreemapEvent::Action(action));
            }
        });
    }
    event
}

impl TreemapState {
    fn refresh_cache(&mut self, panel: Rect, view: &TreemapView<'_>) {
        let area = [
            panel.min.x.round() as i32,
            panel.min.y.round() as i32,
            panel.width().floor() as i32,
            panel.height().floor() as i32,
        ];
        let key = CacheKey {
            zoom: self.zoom,
            area,
            use_logical: view.use_logical,
            generation: view.generation,
            highlighted_ext: view.highlighted_ext,
        };
        if self.cache.as_ref().is_some_and(|cache| cache.key == key) {
            return;
        }
        let bounds = Bounds::new(
            area[0] as f32,
            area[1] as f32,
            area[2] as f32,
            area[3] as f32,
        );
        let rects = layout(view.arena, self.zoom, bounds, view.use_logical);
        let mesh = Arc::new(build_mesh(&rects, view));
        let labels = treemap_labels::collect(
            &rects,
            view.arena,
            |rect| fill_color(rect, view),
            |rect| to_rect(rect.bounds, 0.0),
        );
        self.cache = Some(Cache {
            key,
            rects,
            mesh,
            labels,
        });
    }
}

fn find_rect(rects: &[TreemapRect], id: NodeId) -> Option<Rect> {
    rects
        .iter()
        .find(|rect| rect.id == id)
        .map(|rect| to_rect(rect.bounds, 0.0))
}

fn tooltip(ui: &mut Ui, arena: &Arena, id: NodeId) {
    let node = &arena.nodes[id as usize];
    ui.strong(arena.path_of(id));
    ui.label(format!("Allocated: {}", format_size(node.allocated_size)));
    ui.label(format!("Logical: {}", format_size(node.logical_size)));
    ui.label(format!("Modified: {}", format_mtime(node.mtime_secs)));
}
