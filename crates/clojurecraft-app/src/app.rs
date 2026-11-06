//! The application shell: function library on the left, specification in the centre, and
//! implementation and results on the right.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clojurecraft_domain::{FunctionId, SecretRegistry};
use clojurecraft_store::{Autosaver, FunctionSummary};
use egui::{Id, Key, RichText, Ui};

use crate::editor::{self, Action, EditorContext, ExpandTarget, Focus, Undo};
use crate::edn_view::EdnCache;
use crate::implementation::{self, Tab};
use crate::jobs::{Event, Jobs};
use crate::keys::{self, DUPLICATE, FIND, NEW_FUNCTION, RENAME, SAVE};
use crate::library::{Command, Library, LibraryEvent};
use crate::session::{self, OpenDocument};
use crate::theme;
use crate::views::{self, SystemInfo, TrashAction, WelcomeAction};

/// Toasts stay this long after the pointer or keyboard focus last left them.
const TOAST_LIFETIME: Duration = Duration::from_secs(12);
/// The centre column never gets narrower than this; side columns give way first.
const MIN_CENTER_WIDTH: f32 = 440.0;
const STORAGE_LAST_FUNCTION: &str = "last-function";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Welcome,
    Function(FunctionId),
    Trash,
    Knowledge,
    Settings,
}

enum LibraryState {
    Opening,
    Ready,
    Unavailable(String),
}

struct Toast {
    message: String,
    undo: Option<Undo>,
    error: bool,
    shown_at: Instant,
}

enum Dialog {
    Purge(FunctionSummary),
    Recover(FunctionId),
    QuitWithUnsaved,
}

pub struct ClojureCraftApp {
    jobs: Jobs,
    library: Library,
    autosaver: Option<Autosaver>,
    library_state: LibraryState,
    functions: Vec<FunctionSummary>,
    trash: Vec<FunctionSummary>,
    search: String,
    view: View,
    /// Function to open once the library is listed (restored from the previous session).
    reopen: Option<FunctionId>,
    docs: HashMap<FunctionId, OpenDocument>,
    loading: HashSet<FunctionId>,
    open_errors: HashMap<FunctionId, (String, bool)>,
    system: Option<SystemInfo>,
    toasts: Vec<Toast>,
    dialog: Option<Dialog>,
    edn: EdnCache,
    focus: Option<Focus>,
    expanded: Option<ExpandTarget>,
    right_tab: Tab,
    allow_close: bool,
}

impl ClojureCraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let jobs = Jobs::new(cc.egui_ctx.clone());
        jobs.check_system();
        // Every credential the application handles is registered here so it can never be
        // written into function history or results.
        let library = Library::start(
            jobs.sender(),
            cc.egui_ctx.clone(),
            Arc::new(SecretRegistry::new()),
        );
        let reopen = cc
            .storage
            .and_then(|s| eframe::get_value::<Option<FunctionId>>(s, STORAGE_LAST_FUNCTION))
            .flatten();
        Self {
            jobs,
            library,
            autosaver: None,
            library_state: LibraryState::Opening,
            functions: Vec::new(),
            trash: Vec::new(),
            search: String::new(),
            view: View::Welcome,
            reopen,
            docs: HashMap::new(),
            loading: HashSet::new(),
            open_errors: HashMap::new(),
            system: None,
            toasts: Vec::new(),
            dialog: None,
            edn: EdnCache::default(),
            focus: None,
            expanded: None,
            right_tab: Tab::default(),
            allow_close: false,
        }
    }

    // ---- events ---------------------------------------------------------------------------

    fn handle_events(&mut self) {
        for event in self.jobs.drain() {
            match event {
                Event::SystemChecked {
                    report,
                    budget,
                    runtime,
                } => {
                    self.system = Some(SystemInfo {
                        report: *report,
                        budget,
                        runtime,
                    });
                }
                Event::Library(event) => self.library_event(event),
            }
        }
    }

    fn library_event(&mut self, event: LibraryEvent) {
        match event {
            LibraryEvent::Ready(autosaver) => {
                self.autosaver = Some(autosaver);
                self.library_state = LibraryState::Ready;
            }
            LibraryEvent::Unavailable(e) => self.library_state = LibraryState::Unavailable(e),
            LibraryEvent::Listed { active, trash } => {
                self.functions = active;
                self.trash = trash;
                // Keep names and descriptions typed since the listing was read.
                for f in &mut self.functions {
                    if let Some(open) = self.docs.get(&f.id) {
                        f.name.clone_from(&open.doc.name);
                        f.description.clone_from(&open.doc.description);
                    }
                }
                if let Some(id) = self.reopen.take()
                    && self.functions.iter().any(|f| f.id == id)
                {
                    self.open(id);
                }
            }
            LibraryEvent::Opened { draft, created } => {
                let id = draft.document.id;
                self.loading.remove(&id);
                self.open_errors.remove(&id);
                // The in-memory copy is never older than the stored draft.
                self.docs.entry(id).or_insert_with(|| OpenDocument::new(*draft));
                self.view = View::Function(id);
                if created {
                    self.focus = Some(Focus::Name);
                }
            }
            LibraryEvent::OpenFailed {
                id,
                error,
                recoverable,
            } => {
                self.loading.remove(&id);
                self.open_errors.insert(id, (error, recoverable));
            }
            LibraryEvent::Trashed { id, name } => {
                if self.view == View::Function(id) {
                    self.view = View::Welcome;
                }
                self.toast(
                    format!("Moved “{}” to the trash", views::display_name(&name)),
                    Some(Undo::Function(id)),
                );
            }
            LibraryEvent::Purged(id) => {
                self.docs.remove(&id);
                self.toast("Deleted permanently".into(), None);
            }
            LibraryEvent::Failed { action, error } => {
                self.toasts.push(Toast {
                    message: format!("Could not {action}: {error}"),
                    undo: None,
                    error: true,
                    shown_at: Instant::now(),
                });
            }
            LibraryEvent::Autosave(event) => {
                let id = match &event {
                    clojurecraft_store::AutosaveEvent::Saved { function, .. }
                    | clojurecraft_store::AutosaveEvent::Superseded { function, .. }
                    | clojurecraft_store::AutosaveEvent::Failed { function, .. } => *function,
                };
                if let Some(open) = self.docs.get_mut(&id) {
                    open.apply(&event);
                }
            }
        }
    }

    fn toast(&mut self, message: String, undo: Option<Undo>) {
        self.toasts.push(Toast {
            message,
            undo,
            error: false,
            shown_at: Instant::now(),
        });
    }

    // ---- library operations ---------------------------------------------------------------

    fn ready(&self) -> bool {
        matches!(self.library_state, LibraryState::Ready)
    }

    fn open(&mut self, id: FunctionId) {
        self.view = View::Function(id);
        self.expanded = None;
        if !self.docs.contains_key(&id) && self.loading.insert(id) {
            self.open_errors.remove(&id);
            self.library.send(Command::Open(id));
        }
    }

    fn create(&mut self) {
        if !self.ready() {
            return;
        }
        let name = session::unique_name(
            "Untitled function",
            self.functions.iter().map(|f| f.name.as_str()),
        );
        self.library.send(Command::Create(name));
    }

    fn duplicate(&mut self, id: FunctionId) {
        let Some(open) = self.docs.get(&id) else {
            // Never opened in this session, so the stored draft is the newest version.
            let name = self
                .functions
                .iter()
                .find(|f| f.id == id)
                .map(|f| session::copy_name(&f.name, self.functions.iter().map(|f| f.name.as_str())))
                .unwrap_or_default();
            self.library.send(Command::Duplicate { id, name });
            return;
        };
        let mut copy = open.doc.clone();
        copy.id = FunctionId::generate();
        copy.name = session::copy_name(&open.doc.name, self.functions.iter().map(|f| f.name.as_str()));
        self.library.send(Command::Insert(Box::new(copy)));
    }

    fn trash_function(&mut self, id: FunctionId) {
        let name = self
            .docs
            .get(&id)
            .map(|o| o.doc.name.clone())
            .or_else(|| self.functions.iter().find(|f| f.id == id).map(|f| f.name.clone()))
            .unwrap_or_default();
        self.library.send(Command::Trash { id, name });
    }

    /// Hands the document to the autosaver under a new edit sequence.
    fn submit(&mut self, id: FunctionId, retry: bool) {
        let Some(open) = self.docs.get_mut(&id) else {
            return;
        };
        let seq = if retry { open.retry() } else { open.edited() };
        if let Some(autosaver) = &self.autosaver {
            autosaver.submit(open.doc.clone(), seq);
        }
        if let Some(summary) = self.functions.iter_mut().find(|f| f.id == id) {
            summary.name.clone_from(&open.doc.name);
            summary.description.clone_from(&open.doc.description);
        }
    }

    fn undo(&mut self, undo: Undo) {
        match undo {
            Undo::Function(id) => {
                self.library.send(Command::Restore(id));
                self.open(id);
            }
            Undo::Step {
                function,
                index,
                step,
            } => {
                let step_id = step.id;
                if let Some(open) = self.docs.get_mut(&function)
                    && open.doc.insert_step(index, step)
                {
                    self.focus = Some(Focus::Step(step_id));
                    self.submit(function, false);
                }
            }
            Undo::Example {
                function,
                index,
                example,
            } => {
                if let Some(open) = self.docs.get_mut(&function) {
                    let example_id = example.id;
                    if open.doc.insert_example(index, *example) {
                        open.view.selected_example = Some(example_id);
                        self.submit(function, false);
                    }
                }
            }
            Undo::Contract { function, contract } => {
                if let Some(open) = self.docs.get_mut(&function)
                    && open.doc.contract.is_none()
                {
                    open.doc.contract = Some(*contract);
                    self.submit(function, false);
                }
            }
        }
    }

    fn current_function(&self) -> Option<FunctionId> {
        match self.view {
            View::Function(id) if self.docs.contains_key(&id) => Some(id),
            _ => None,
        }
    }

    // ---- input ----------------------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() || self.expanded.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&NEW_FUNCTION)) {
            self.create();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&FIND)) {
            self.focus = Some(Focus::Search);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SAVE))
            && let Some(autosaver) = &self.autosaver
        {
            autosaver.flush();
        }
        if let Some(id) = self.current_function() {
            if ctx.input_mut(|i| i.consume_shortcut(&DUPLICATE)) {
                self.duplicate(id);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&RENAME)) {
                self.focus = Some(Focus::Name);
            }
        }
    }

    /// Before closing, writes pending edits; if any could not be saved, asks first.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.allow_close {
            return;
        }
        let flushed = self.autosaver.as_ref().is_none_or(Autosaver::flush);
        let problem = self.docs.values().any(|d| d.status().is_problem());
        if !flushed || problem {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::QuitWithUnsaved);
        }
    }

    // ---- columns --------------------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut Ui) {
        let mut open = None;
        let mut actions: Vec<(FunctionId, &'static str)> = Vec::new();
        egui::Panel::bottom("navigation")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 8)))
            .show(ui, |ui| {
                ui.separator();
                let trash_label = if self.trash.is_empty() {
                    "Trash".to_owned()
                } else {
                    format!("Trash ({})", self.trash.len())
                };
                for (view, label) in [
                    (View::Knowledge, "Knowledge sources".to_owned()),
                    (View::Settings, "Settings".to_owned()),
                    (View::Trash, trash_label),
                ] {
                    let r = list_item(ui, self.view == view, &label);
                    if r.clicked() {
                        self.view = view;
                    }
                }
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("Functions");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = ui
                    .add_enabled(self.ready(), egui::Button::new("+ New"))
                    .on_hover_text(format!("New function ({})", keys::label(&NEW_FUNCTION)));
                if r.clicked() {
                    self.create();
                }
            });
        });
        ui.add_space(4.0);
        let search = ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .id(Id::unique("search"))
                .hint_text(format!("Search  {}", keys::label(&FIND)))
                .desired_width(f32::INFINITY),
        );
        if self.focus == Some(Focus::Search) {
            self.focus = None;
            search.request_focus();
        }
        if search.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.search.clear();
        }
        let matching: Vec<&FunctionSummary> = self
            .functions
            .iter()
            .filter(|f| session::matches_search(f, &self.search))
            .collect();
        if search.lost_focus()
            && ui.input(|i| i.key_pressed(Key::Enter))
            && let Some(first) = matching.first()
        {
            open = Some(first.id);
        }
        ui.add_space(6.0);
        match &self.library_state {
            LibraryState::Opening => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Opening library…").color(theme::MUTED));
                });
            }
            LibraryState::Unavailable(_) => {
                ui.label(RichText::new("The function library could not be opened.").color(theme::BAD));
            }
            LibraryState::Ready if self.functions.is_empty() => {
                ui.label(RichText::new("No saved functions yet.").color(theme::MUTED));
            }
            LibraryState::Ready if matching.is_empty() => {
                ui.label(RichText::new("No functions match the search.").color(theme::MUTED));
            }
            LibraryState::Ready => {
                egui::ScrollArea::vertical()
                    .id_salt("functions")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for f in matching {
                            let selected = self.view == View::Function(f.id);
                            let r = list_item(ui, selected, views::display_name(&f.name)).on_hover_text(
                                if f.description.trim().is_empty() {
                                    views::display_name(&f.name).to_owned()
                                } else {
                                    f.description.clone()
                                },
                            );
                            if r.clicked() {
                                open = Some(f.id);
                            }
                            r.context_menu(|ui| {
                                if ui.button("Open").clicked() {
                                    open = Some(f.id);
                                }
                                if ui.button("Rename").clicked() {
                                    actions.push((f.id, "rename"));
                                }
                                if ui.button("Duplicate").clicked() {
                                    actions.push((f.id, "duplicate"));
                                }
                                if ui.button("Move to trash").clicked() {
                                    actions.push((f.id, "trash"));
                                }
                            });
                        }
                    });
            }
        }
        if let Some(id) = open {
            self.open(id);
        }
        for (id, action) in actions {
            match action {
                "rename" => {
                    self.open(id);
                    self.focus = Some(Focus::Name);
                }
                "duplicate" => self.duplicate(id),
                _ => self.trash_function(id),
            }
        }
    }

    fn center(&mut self, ui: &mut Ui) {
        match self.view {
            View::Welcome => {
                let error = match &self.library_state {
                    LibraryState::Unavailable(e) => Some(e.as_str()),
                    _ => None,
                };
                if let WelcomeAction::Create = views::welcome(ui, error) {
                    self.create();
                }
            }
            View::Trash => match views::trash(ui, &self.trash) {
                Some(TrashAction::Restore(f)) => self.library.send(Command::Restore(f.id)),
                Some(TrashAction::Purge(f)) => self.dialog = Some(Dialog::Purge(f)),
                None => {}
            },
            View::Knowledge => views::knowledge(ui, self.system.as_ref()),
            View::Settings => views::settings(ui, self.system.as_ref()),
            View::Function(id) => self.function_editor(ui, id),
        }
    }

    fn function_editor(&mut self, ui: &mut Ui, id: FunctionId) {
        if let Some((error, recoverable)) = self.open_errors.get(&id).cloned() {
            views::page_title(ui, "This function could not be opened", "");
            ui.label(RichText::new(error).color(theme::BAD));
            ui.add_space(8.0);
            if recoverable && ui.button("Replace the draft with the latest revision…").clicked() {
                self.dialog = Some(Dialog::Recover(id));
            }
            return;
        }
        let Some(open) = self.docs.get_mut(&id) else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Opening…").color(theme::MUTED));
            });
            return;
        };
        let mut actions = Vec::new();
        let changed = egui::ScrollArea::vertical()
            .id_salt(("editor", id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Keep the editor readable on very wide windows.
                ui.set_max_width(ui.available_width().min(980.0));
                let mut cx = EditorContext {
                    cache: &mut self.edn,
                    focus: &mut self.focus,
                    expanded: &mut self.expanded,
                    actions: &mut actions,
                };
                editor::show(ui, open, &mut cx)
            })
            .inner;
        if changed {
            self.submit(id, false);
        }
        for action in actions {
            match action {
                Action::Duplicate => self.duplicate(id),
                Action::Trash => self.trash_function(id),
                Action::RetrySave => self.submit(id, true),
                Action::Removed { message, undo } => self.toast(message, Some(undo)),
            }
        }
    }

    // ---- overlays -------------------------------------------------------------------------

    fn toasts(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts
            .retain(|t| now.duration_since(t.shown_at) < TOAST_LIFETIME);
        if self.toasts.is_empty() {
            return;
        }
        // Keep repainting so toasts disappear on time even without input.
        ctx.request_repaint_after(Duration::from_millis(500));
        let mut undo = None;
        let mut dismiss = None;
        let mut engaged = false;
        egui::Area::new(Id::unique("toasts"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -18.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for (i, toast) in self.toasts.iter().enumerate().rev().take(3) {
                    egui::Frame::new()
                        .fill(theme::CONTROL)
                        .stroke(egui::Stroke::new(
                            1.0,
                            if toast.error {
                                theme::BAD
                            } else {
                                theme::LINE_STRONG
                            },
                        ))
                        .corner_radius(theme::RADIUS)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let color = if toast.error { theme::BAD } else { theme::INK };
                                ui.label(RichText::new(&toast.message).color(color));
                                if toast.undo.is_some() {
                                    let r = ui.button("Undo");
                                    engaged |= r.has_focus();
                                    if r.clicked() {
                                        undo = Some(i);
                                    }
                                }
                                let r = ui.small_button("Dismiss");
                                engaged |= r.has_focus();
                                if r.clicked() {
                                    dismiss = Some(i);
                                }
                            });
                        });
                    ui.add_space(4.0);
                }
                engaged |= ui.ui_contains_pointer();
            });
        // Hold toasts open while someone is reaching for them.
        if engaged {
            for t in &mut self.toasts {
                t.shown_at = now;
            }
        }
        if let Some(i) = undo {
            let toast = self.toasts.remove(i);
            if let Some(u) = toast.undo {
                self.undo(u);
            }
        } else if let Some(i) = dismiss {
            self.toasts.remove(i);
        }
    }

    fn dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let mut close = false;
        let mut confirmed = false;
        let (title, body, confirm) = match dialog {
            Dialog::Purge(f) => (
                "Delete permanently?".to_owned(),
                format!(
                    "“{}” and its entire history will be deleted. This cannot be undone.",
                    views::display_name(&f.name)
                ),
                "Delete permanently",
            ),
            Dialog::Recover(_) => (
                "Replace the unreadable draft?".to_owned(),
                "The draft will be replaced by the latest saved revision. The unreadable draft \
                 cannot be recovered afterwards."
                    .to_owned(),
                "Replace draft",
            ),
            Dialog::QuitWithUnsaved => (
                "Quit without saving?".to_owned(),
                "Some changes could not be saved and will be lost if you quit now.".to_owned(),
                "Quit anyway",
            ),
        };
        let response = egui::Modal::new(Id::unique("dialog")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading(title);
            ui.add_space(6.0);
            ui.label(body);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let cancel = ui.button("Cancel");
                if ui.button(RichText::new(confirm).color(theme::BAD)).clicked() {
                    confirmed = true;
                }
                // The safe choice has focus, so Enter does not destroy anything by accident.
                if !ui.memory(|m| m.focused().is_some()) {
                    cancel.request_focus();
                }
                if cancel.clicked() {
                    close = true;
                }
            });
        });
        if response.should_close() {
            close = true;
        }
        if confirmed {
            match self.dialog.take() {
                Some(Dialog::Purge(f)) => self.library.send(Command::Purge(f.id)),
                Some(Dialog::Recover(id)) => {
                    self.open_errors.remove(&id);
                    self.loading.insert(id);
                    self.library.send(Command::Recover(id));
                }
                Some(Dialog::QuitWithUnsaved) => {
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                None => {}
            }
        } else if close {
            self.dialog = None;
        }
    }
}

/// A full-width, left-aligned selectable row.
fn list_item(ui: &mut Ui, selected: bool, text: &str) -> egui::Response {
    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
        ui.add(
            egui::Button::selectable(selected, text)
                .truncate()
                .min_size(egui::vec2(0.0, theme::CONTROL_H)),
        )
    })
    .inner
}

impl eframe::App for ClojureCraftApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_events();
        self.handle_close(&ctx);
        self.shortcuts(&ctx);

        let total = ctx.content_rect().width();
        let side = egui::Frame::new()
            .fill(theme::PANEL)
            .inner_margin(12.0)
            .stroke(theme::hairline());
        let left_max = (total - MIN_CENTER_WIDTH - 300.0).clamp(200.0, 360.0);
        let left = egui::Panel::left("library")
            .resizable(true)
            .default_size(240.0)
            .size_range(200.0..=left_max)
            .frame(side)
            .show(ui, |ui| self.sidebar(ui))
            .response
            .rect
            .right();
        let right_max = (total - left - MIN_CENTER_WIDTH).clamp(300.0, 720.0);
        let current = match self.view {
            View::Function(id) => self.docs.get(&id).map(|o| &o.doc),
            _ => None,
        };
        egui::Panel::right("implementation")
            .resizable(true)
            .default_size(400.0)
            .size_range(300.0..=right_max)
            .frame(side)
            .show(ui, |ui| implementation::show(ui, &mut self.right_tab, current));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(20.0))
            .show(ui, |ui| self.center(ui));

        self.toasts(&ctx);
        self.dialog(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let last = match self.view {
            View::Function(id) => Some(id),
            _ => None,
        };
        eframe::set_value(storage, STORAGE_LAST_FUNCTION, &last);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(autosaver) = &self.autosaver
            && !autosaver.flush()
        {
            tracing::error!("some edits could not be saved before exit");
        }
    }
}
