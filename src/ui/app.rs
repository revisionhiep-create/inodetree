//! `eframe::App` state machine: background scan, deletions and panel layout.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};

use eframe::egui::{self, Event, Key, Modifiers};

use super::background::dedupe_delete_targets;
use super::context_menu::RowAction;
use super::session::Session;
use super::toolbar::FILTER_FIELD_ID;
use super::treemap::TreemapEvent;
use crate::actions::{ActionResult, DeleteMode};
use crate::arena::NodeId;
use crate::mounts::MountInfo;
use crate::scan::job::ScanJob;

pub struct App {
    pub(super) context: egui::Context,
    pub(super) path_input: String,
    pub(super) filter_input: String,
    pub(super) mounts: Vec<MountInfo>,
    pub(super) mounts_sender: Sender<Vec<MountInfo>>,
    pub(super) mounts_receiver: Receiver<Vec<MountInfo>>,
    pub(super) job: Option<ScanJob>,
    pub(super) session: Option<Session>,
    pub(super) use_logical: bool,
    pub(super) error: Option<String>,
    pub(super) confirming_delete: Vec<NodeId>,
    // Ids of deletions in flight, so a rescan in between cannot apply them to a new arena
    pub(super) pending_deletes: HashSet<NodeId>,
    pub(super) action_sender: Sender<ActionResult>,
    pub(super) action_receiver: Receiver<ActionResult>,
}

impl App {
    pub fn new(context: &egui::Context, initial_path: Option<&Path>) -> Self {
        let (mounts_sender, mounts_receiver) = mpsc::channel();
        let (action_sender, action_receiver) = mpsc::channel();
        let mut app = Self {
            context: context.clone(),
            path_input: initial_path
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            filter_input: String::new(),
            mounts: Vec::new(),
            mounts_sender,
            mounts_receiver,
            job: None,
            session: None,
            use_logical: false,
            error: None,
            confirming_delete: Vec::new(),
            pending_deletes: HashSet::new(),
            action_sender,
            action_receiver,
        };
        app.load_mounts();
        if initial_path.is_some() {
            app.start_scan();
        }
        app
    }

    fn handle_treemap_event(&mut self, event: Option<TreemapEvent>) {
        match (event, &mut self.session) {
            (Some(TreemapEvent::Select(id, mode)), Some(session)) => {
                session.tree.select_and_reveal(&session.arena, id, mode);
            }
            (Some(TreemapEvent::SecondarySelect(id)), Some(session)) => {
                session.tree.ensure_selected_for_menu(id);
            }
            (Some(TreemapEvent::Action(action)), _) => self.apply_action(action),
            _ => {}
        }
    }

    /// Keeps the session in step with the search box, re-filtering only when the text changed.
    fn sync_filter(&mut self) {
        if let Some(session) = &mut self.session {
            if session.applied_filter != self.filter_input {
                session.apply_filter(&self.filter_input);
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Checked before the keyboard-focus guard so Ctrl+F also works while typing a path
        if ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::F)) {
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(FILTER_FIELD_ID)));
        }
        if !self.confirming_delete.is_empty() || ctx.wants_keyboard_input() {
            return;
        }
        let selected = self
            .session
            .as_ref()
            .and_then(|session| session.tree.selected_in_order().into_iter().next());
        let (permanent, trash, copy, rescan) = ctx.input_mut(|input| {
            // `egui-winit` translates Shift+Delete on Windows into `Event::Cut` without a Delete key event.
            let shift_delete_cut = cfg!(windows)
                && input.modifiers.shift
                && !input.modifiers.command
                && input.events.iter().any(|event| matches!(event, Event::Cut));
            if shift_delete_cut {
                input.events.retain(|event| !matches!(event, Event::Cut));
            }
            (
                input.consume_key(Modifiers::SHIFT, Key::Delete) || shift_delete_cut,
                input.consume_key(Modifiers::NONE, Key::Delete),
                input
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Copy)),
                input.consume_key(Modifiers::NONE, Key::F5),
            )
        });
        match (selected, permanent, trash, copy) {
            (Some(id), true, _, _) => self.apply_action(RowAction::DeletePermanently(id)),
            (Some(id), _, true, _) => self.apply_action(RowAction::MoveToTrash(id)),
            (Some(id), _, _, true) => self.apply_action(RowAction::CopyPath(id)),
            _ => {}
        }
        if rescan {
            // Drives mounted while the app is open only show up in the picker after a reload
            self.load_mounts();
            self.start_scan();
        }
    }

    fn apply_action(&mut self, action: RowAction) {
        match action {
            RowAction::OpenInFileManager(id) => self.open_in_file_manager(id),
            RowAction::CopyPath(id) => {
                if let Some(session) = &self.session {
                    let targets = action_targets(session, id);
                    let sep = if cfg!(windows) { "\r\n" } else { "\n" };
                    let text: Vec<String> = targets
                        .into_iter()
                        .map(|target| session.arena.path_of(target))
                        .collect();
                    self.context.copy_text(text.join(sep));
                }
            }
            RowAction::MoveToTrash(id) => {
                let Some(session) = &self.session else { return };
                let targets = dedupe_delete_targets(&session.arena, &action_targets(session, id));
                if !targets.is_empty() {
                    self.start_delete_many(&targets, DeleteMode::Trash);
                }
            }
            RowAction::DeletePermanently(id) => {
                let Some(session) = &self.session else { return };
                let targets = dedupe_delete_targets(&session.arena, &action_targets(session, id));
                if !targets.is_empty() {
                    self.confirming_delete = targets;
                }
            }
        }
    }
}

fn action_targets(session: &Session, id: NodeId) -> Vec<NodeId> {
    if session.tree.selected.contains(&id) {
        session.tree.selected_in_order()
    } else {
        vec![id]
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_mounts();
        self.poll_scan();
        self.poll_actions();
        self.handle_shortcuts(ctx);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| self.show_toolbar(ui));
        self.sync_filter();
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.show_status(ui));
        if self.session.is_some() {
            let treemap_event = egui::TopBottomPanel::bottom("treemap")
                .resizable(true)
                .default_height(ctx.screen_rect().height() * 0.5)
                .min_height(120.0)
                .show(ctx, |ui| self.show_treemap(ui))
                .inner;
            self.handle_treemap_event(treemap_event);
            egui::SidePanel::right("extensions")
                .resizable(true)
                .default_width(ctx.screen_rect().width() * 0.32)
                .show(ctx, |ui| self.show_extensions(ui));
        }
        let action = egui::CentralPanel::default()
            .show(ctx, |ui| self.show_tree(ui))
            .inner;
        if let Some(action) = action {
            self.apply_action(action);
        }
        self.show_delete_confirmation(ctx);
    }
}
