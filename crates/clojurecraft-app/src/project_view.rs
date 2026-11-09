//! The project tree in the sidebar (namespaces and their functions), and the dialogs for opening
//! a project and adding a namespace or a function.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use egui::{Id, Key, RichText, Ui};

use crate::project::{FunctionLeaf, Project, TreeNode};
use crate::theme;

pub enum TreeAction {
    /// Open a function's specification.
    Open { namespace: String, function: String },
    /// Select a namespace or prefix (where + New adds a function or a namespace).
    Select(String),
}

/// What is selected in the tree: a namespace (or prefix), or a function in a namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub path: Option<String>,
    pub function: Option<String>,
}

pub fn tree(ui: &mut Ui, project: &Project, selected: &Selection) -> Option<TreeAction> {
    let mut action = None;
    let nodes = project.tree();
    if nodes.is_empty() {
        ui.label(RichText::new("No namespaces yet. Add one with + New.").color(theme::MUTED));
        return None;
    }
    egui::ScrollArea::vertical()
        .id_salt("namespaces")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for n in &nodes {
                node(ui, n, selected, &mut action);
            }
        });
    action
}

fn indent(ui: &mut Ui) {
    ui.add_space(ui.spacing().icon_width + ui.spacing().item_spacing.x);
}

fn node(ui: &mut Ui, n: &TreeNode, selected: &Selection, action: &mut Option<TreeAction>) {
    if n.children.is_empty() && n.functions.is_empty() {
        ui.horizontal(|ui| {
            indent(ui);
            row(ui, n, selected, action);
        });
        return;
    }
    let id = ui.make_persistent_id(("namespace-tree", &n.path));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
        .show_header(ui, |ui| row(ui, n, selected, action))
        .body(|ui| {
            for f in &n.functions {
                ui.horizontal(|ui| {
                    indent(ui);
                    leaf(ui, n, f, selected, action);
                });
            }
            for child in &n.children {
                node(ui, child, selected, action);
            }
        });
}

fn row(ui: &mut Ui, n: &TreeNode, selected: &Selection, action: &mut Option<TreeAction>) {
    let is_selected = selected.function.is_none() && selected.path.as_deref() == Some(n.path.as_str());
    let mut text = RichText::new(&n.segment);
    let hover = if n.namespace {
        format!("{} ({} functions)", n.path, n.functions.len())
    } else {
        text = text.italics().color(theme::MUTED);
        format!("{} (not a namespace itself)", n.path)
    };
    let r = ui
        .add(egui::Button::selectable(is_selected, text).truncate())
        .on_hover_text(hover);
    if r.clicked() {
        *action = Some(TreeAction::Select(n.path.clone()));
    }
}

fn leaf(ui: &mut Ui, n: &TreeNode, f: &FunctionLeaf, selected: &Selection, action: &mut Option<TreeAction>) {
    let is_selected = selected.path.as_deref() == Some(n.path.as_str())
        && selected.function.as_deref() == Some(f.name.as_str());
    let mut text = RichText::new(&f.name).monospace();
    let hover = if f.defined {
        format!("{}/{}", n.path, f.name)
    } else {
        text = text.color(theme::MUTED);
        format!("{}/{} (no code yet)", n.path, f.name)
    };
    let r = ui
        .add(egui::Button::selectable(is_selected, text).truncate())
        .on_hover_text(hover);
    if r.clicked() {
        *action = Some(TreeAction::Open {
            namespace: n.path.clone(),
            function: f.name.clone(),
        });
    }
}

pub enum ProjectDialog {
    Open {
        path: String,
        error: Option<String>,
        /// A folder picker is showing.
        picking: bool,
    },
    NewNamespace {
        name: String,
        error: Option<String>,
    },
    NewFunction {
        namespace: String,
        name: String,
        arguments: u32,
        error: Option<String>,
    },
}

impl ProjectDialog {
    pub fn open(path: String) -> Self {
        Self::Open {
            path,
            error: None,
            picking: false,
        }
    }

    /// A new namespace below `parent`.
    pub fn new_namespace(parent: Option<&str>) -> Self {
        Self::NewNamespace {
            name: parent.map(|p| format!("{p}.")).unwrap_or_default(),
            error: None,
        }
    }

    pub fn new_function(namespace: String) -> Self {
        Self::NewFunction {
            namespace,
            name: String::new(),
            arguments: 1,
            error: None,
        }
    }

    pub fn fail(&mut self, message: String) {
        match self {
            Self::Open { error, .. } | Self::NewNamespace { error, .. } | Self::NewFunction { error, .. } => {
                *error = Some(message);
            }
        }
    }
}

pub enum DialogAction {
    None,
    Close,
    Browse,
    Open(PathBuf),
    CreateNamespace(String),
    CreateFunction {
        namespace: String,
        name: String,
        arguments: u32,
    },
}

pub fn dialog(ctx: &egui::Context, dialog: &mut ProjectDialog) -> DialogAction {
    let mut action = DialogAction::None;
    let response = egui::Modal::new(Id::unique("project-dialog")).show(ctx, |ui| {
        ui.set_width(460.0);
        let (title, text, error, hint, confirm) = match dialog {
            ProjectDialog::Open { path, error, .. } => (
                "Open a Clojure project".to_owned(),
                path,
                error,
                "/path/to/project",
                "Open",
            ),
            ProjectDialog::NewNamespace { name, error } => (
                "New namespace".to_owned(),
                name,
                error,
                "my-app.orders.totals",
                "Create",
            ),
            ProjectDialog::NewFunction {
                namespace,
                name,
                error,
                ..
            } => (
                format!("New function in {namespace}"),
                name,
                error,
                "order-total",
                "Create",
            ),
        };
        ui.heading(title);
        ui.add_space(6.0);
        let field = ui.add(
            egui::TextEdit::singleline(text)
                .id(Id::unique("project-dialog-field"))
                .hint_text(hint)
                .desired_width(f32::INFINITY),
        );
        if !ui.memory(|m| m.focused().is_some()) {
            field.request_focus();
        }
        let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let value = text.trim().to_owned();
        if let Some(e) = error {
            ui.label(RichText::new(e.as_str()).color(theme::BAD));
        }
        if let ProjectDialog::NewFunction { arguments, .. } = dialog {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Arguments");
                ui.add(egui::DragValue::new(arguments).range(0..=8));
            });
            if *arguments != 1 {
                ui.label(
                    RichText::new("Example inputs are then vectors of the arguments, such as [order 0.2].")
                        .small()
                        .color(theme::MUTED),
                );
            }
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                action = DialogAction::Close;
            }
            if let ProjectDialog::Open { picking, .. } = dialog
                && ui.add_enabled(!*picking, egui::Button::new("Browse…")).clicked()
            {
                action = DialogAction::Browse;
            }
            let confirmed = ui
                .add_enabled(!value.is_empty(), egui::Button::new(confirm))
                .clicked();
            if (confirmed || submitted) && !value.is_empty() {
                action = match dialog {
                    ProjectDialog::Open { .. } => DialogAction::Open(expand_home(&value)),
                    ProjectDialog::NewNamespace { .. } => DialogAction::CreateNamespace(value.clone()),
                    ProjectDialog::NewFunction {
                        namespace, arguments, ..
                    } => DialogAction::CreateFunction {
                        namespace: namespace.clone(),
                        name: value.clone(),
                        arguments: *arguments,
                    },
                };
            }
        });
    });
    if response.should_close() && matches!(action, DialogAction::None) {
        action = DialogAction::Close;
    }
    action
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Shows the operating system's folder picker on a separate thread. The result is `None` when
/// the user cancelled.
pub fn pick_folder(ctx: &egui::Context) -> Receiver<Result<Option<PathBuf>, String>> {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(run_picker());
        ctx.request_repaint();
    });
    rx
}

fn run_picker() -> Result<Option<PathBuf>, String> {
    let output = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"Open a Clojure project\")",
            ])
            .output()
    } else {
        std::process::Command::new("zenity")
            .args([
                "--file-selection",
                "--directory",
                "--title=Open a Clojure project",
            ])
            .output()
    }
    .map_err(|e| format!("No folder picker is available ({e}); type the path instead."))?;
    if !output.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}
