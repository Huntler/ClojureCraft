//! Keyboard shortcuts and their platform-appropriate labels. `COMMAND` is Cmd on macOS and Ctrl
//! elsewhere. Labels use words rather than symbols such as ⌘, which the bundled fonts lack.

use egui::{Key, KeyboardShortcut, ModifierNames, Modifiers};

pub const NEW_FUNCTION: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
pub const FIND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
pub const DUPLICATE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::D);
pub const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
pub const RENAME: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F2);
pub const MOVE_STEP_UP: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowUp);
pub const MOVE_STEP_DOWN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowDown);
pub const ADD_STEP_AFTER: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);

const IS_MAC: bool = cfg!(target_os = "macos");

pub fn label(shortcut: &KeyboardShortcut) -> String {
    shortcut.format(&ModifierNames::NAMES, IS_MAC)
}

/// The command modifier's name: "Cmd" on macOS, "Ctrl" elsewhere.
pub fn command() -> &'static str {
    if IS_MAC { "Cmd" } else { "Ctrl" }
}

/// Rows for the shortcut reference in settings.
pub fn reference() -> Vec<(String, &'static str)> {
    let cmd = command();
    vec![
        (label(&NEW_FUNCTION), "New function"),
        (label(&FIND), "Search functions"),
        (label(&DUPLICATE), "Duplicate the open function"),
        (label(&RENAME), "Rename the open function"),
        (label(&SAVE), "Save now (saving is otherwise automatic)"),
        ("Tab / Shift+Tab".into(), "Move between controls"),
        ("Space / Enter".into(), "Activate the focused button"),
        (
            format!("{} / {}", label(&MOVE_STEP_UP), label(&MOVE_STEP_DOWN)),
            "Move the focused step",
        ),
        (label(&ADD_STEP_AFTER), "Add a step after the focused one"),
        (
            format!("{cmd}+Plus / {cmd}+Minus / {cmd}+0"),
            "Change the text size",
        ),
        (
            "Esc".into(),
            "Close a dialog or the large editor; clear the search",
        ),
    ]
}
