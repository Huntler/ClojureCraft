//! EDN editing and inspection widgets: a syntax-highlighted editor with parse diagnostics that
//! never alters the text, and a collapsible tree inspector for large nested values.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use clojurecraft_edn::{
    Class, Node, NodeKind, Parsed, Severity, char_index, highlight, line_col, parse_value,
};
use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{Color32, FontId, Id, RichText, Stroke, TextStyle, Ui};

use crate::theme::{self, syntax};

/// Beyond this size, text is shown without colouring so typing stays responsive.
const HIGHLIGHT_LIMIT: usize = 256 * 1024;
/// Collection children shown at once in the inspector; more are revealed on request.
const PAGE: usize = 100;
/// Diagnostics listed under an editor; the rest are summarized.
const LISTED_DIAGNOSTICS: usize = 4;

/// Parse results per editor, recomputed only when the text changes.
#[derive(Default)]
pub struct EdnCache {
    entries: HashMap<Id, (u64, Arc<Parsed>)>,
}

impl EdnCache {
    pub fn parsed(&mut self, id: Id, text: &str) -> Arc<Parsed> {
        let hash = text_hash(text);
        match self.entries.get(&id) {
            Some((h, parsed)) if *h == hash => parsed.clone(),
            _ => {
                let parsed = Arc::new(parse_value(text));
                self.entries.insert(id, (hash, parsed.clone()));
                parsed
            }
        }
    }
}

fn text_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

pub fn class_color(class: Class) -> Color32 {
    match class {
        Class::Plain | Class::Symbol => syntax::SYMBOL,
        Class::Comment => syntax::COMMENT,
        Class::Delimiter(d) => syntax::DELIMITERS[usize::from(d) % syntax::DELIMITERS.len()],
        Class::String | Class::Char => syntax::STRING,
        Class::Number => syntax::NUMBER,
        Class::Keyword => syntax::KEYWORD,
        Class::Literal => syntax::LITERAL,
        Class::Tag => syntax::TAG,
        Class::ReaderMacro => syntax::MACRO,
        Class::Invalid => syntax::INVALID,
    }
}

/// Colours `text` for display; `errors` (byte ranges) are underlined.
pub fn highlight_job(text: &str, font: FontId, errors: &[std::ops::Range<usize>], wrap: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    if text.len() > HIGHLIGHT_LIMIT {
        job.append(text, 0.0, TextFormat::simple(font, syntax::SYMBOL));
        return job;
    }
    for (span, class) in highlight(text) {
        let underline = errors
            .iter()
            .any(|e| e.start < span.end.max(span.start + 1) && span.start < e.end.max(e.start + 1));
        job.append(
            &text[span],
            0.0,
            TextFormat {
                font_id: font.clone(),
                color: class_color(class),
                underline: if underline {
                    Stroke::new(1.5, theme::BAD)
                } else {
                    Stroke::NONE
                },
                ..Default::default()
            },
        );
    }
    job
}

pub struct FieldOutput {
    pub changed: bool,
    pub expand: bool,
}

/// An EDN value editor with a status line, a text/tree toggle, and positioned diagnostics.
///
/// `max_height` bounds the editor; longer text scrolls inside it.
pub fn edn_field(
    ui: &mut Ui,
    id: Id,
    label: &str,
    hint: &str,
    text: &mut String,
    cache: &mut EdnCache,
    max_height: f32,
) -> FieldOutput {
    let mut out = FieldOutput {
        changed: false,
        expand: false,
    };
    let tree_id = id.with("tree");
    let mut tree = ui.data(|d| d.get_temp::<bool>(tree_id)).unwrap_or(false);
    let parsed = cache.parsed(id, text);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).strong());
        status_label(ui, &parsed, text);
        ui.add_space(4.0);
        if ui
            .selectable_label(!tree, "Text")
            .on_hover_text("Edit the EDN text")
            .clicked()
        {
            tree = false;
        }
        if ui
            .selectable_label(tree, "Tree")
            .on_hover_text("Inspect the parsed value as a collapsible tree")
            .clicked()
        {
            tree = true;
        }
        if ui
            .button("Expand")
            .on_hover_text("Open a large editor with the data inspector")
            .clicked()
        {
            out.expand = true;
        }
    });
    ui.data_mut(|d| d.insert_temp(tree_id, tree));
    if tree {
        egui::Frame::new()
            .fill(theme::SUNKEN)
            .stroke(theme::hairline())
            .corner_radius(theme::RADIUS)
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .id_salt(id.with("tree-scroll"))
                    .max_height(max_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| inspector(ui, id.with("tree-root"), &parsed, text));
            });
    } else {
        out.changed = editor(ui, id, hint, text, &parsed, max_height);
    }
    diagnostics(ui, id, text, &parsed);
    out
}

/// The text editor itself; returns whether the text changed.
pub fn editor(ui: &mut Ui, id: Id, hint: &str, text: &mut String, parsed: &Parsed, max_height: f32) -> bool {
    editor_sized(ui, id, hint, text, parsed, max_height, false)
}

/// `fill`: take the full `max_height` even when the text is short.
fn editor_sized(
    ui: &mut Ui,
    id: Id,
    hint: &str,
    text: &mut String,
    parsed: &Parsed,
    max_height: f32,
    fill: bool,
) -> bool {
    let font = TextStyle::Monospace.resolve(ui.style());
    let errors: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.span.clone())
        .collect();
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let s = buf.as_str();
        // Diagnostics belong to the text they were computed from; after an edit in this frame
        // they are stale until the next parse, so they are not drawn.
        let errors: &[_] = if s.len() == parsed.source_len {
            &errors
        } else {
            &[]
        };
        let job = highlight_job(s, font.clone(), errors, wrap);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let mut changed = false;
    egui::ScrollArea::vertical()
        .id_salt(id.with("scroll"))
        .max_height(max_height)
        .auto_shrink([false, !fill])
        .show(ui, |ui| {
            let response = egui::TextEdit::multiline(text)
                .id(id)
                .code_editor()
                // Tab moves focus to the next control so the editor is not a keyboard trap.
                .lock_focus(false)
                .hint_text(hint)
                .desired_width(f32::INFINITY)
                .desired_rows(3)
                .layouter(&mut layouter)
                .show(ui)
                .response;
            changed = response.changed();
        });
    changed
}

fn status_label(ui: &mut Ui, parsed: &Parsed, text: &str) {
    if text.trim().is_empty() || (parsed.values.is_empty() && !parsed.has_errors()) {
        ui.label(RichText::new("empty").small().color(theme::MUTED));
    } else if parsed.has_errors() {
        let n = parsed.error_count();
        ui.label(
            RichText::new(format!("{n} {}", if n == 1 { "problem" } else { "problems" }))
                .small()
                .color(theme::BAD),
        );
    } else if let Some(v) = parsed.values.first() {
        ui.label(
            RichText::new(format!("valid · {}", v.summary()))
                .small()
                .color(theme::ACCENT_INK),
        );
    }
}

/// Lists diagnostics; clicking one moves the editor's cursor to it.
fn diagnostics(ui: &mut Ui, id: Id, text: &str, parsed: &Parsed) {
    if parsed.diagnostics.is_empty() {
        return;
    }
    for d in parsed.diagnostics.iter().take(LISTED_DIAGNOSTICS) {
        let (line, col) = line_col(text, d.span.start);
        let color = match d.severity {
            Severity::Error => theme::BAD,
            Severity::Warning => theme::WARN,
        };
        let r = ui
            .add(
                egui::Label::new(
                    RichText::new(format!("Line {line}, column {col}: {}", d.message))
                        .small()
                        .color(color),
                )
                .sense(egui::Sense::click()),
            )
            .on_hover_text("Show in editor");
        if r.clicked() {
            reveal(ui, id, text, d.span.clone());
        }
    }
    let more = parsed.diagnostics.len().saturating_sub(LISTED_DIAGNOSTICS) + parsed.suppressed;
    if more > 0 {
        ui.label(
            RichText::new(format!("…and {more} more"))
                .small()
                .color(theme::MUTED),
        );
    }
}

/// Switches the field to text mode, selects `span`, and focuses the editor.
fn reveal(ui: &Ui, id: Id, text: &str, span: std::ops::Range<usize>) {
    ui.data_mut(|d| d.insert_temp(id.with("tree"), false));
    let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
    let start = char_index(text, span.start);
    let end = char_index(text, span.end).max(start);
    state
        .cursor
        .set_char_range(Some(CCursorRange::two(CCursor::new(start), CCursor::new(end))));
    state.store(ui.ctx(), id);
    ui.memory_mut(|m| m.request_focus(id));
}

/// Collapsible tree of the parsed value.
pub fn inspector(ui: &mut Ui, id: Id, parsed: &Parsed, src: &str) {
    match parsed.values.first() {
        None if parsed.has_errors() => {
            ui.label(
                RichText::new("Nothing could be read; fix the problems listed below.").color(theme::MUTED),
            );
        }
        None => {
            ui.label(RichText::new("No value yet.").color(theme::MUTED));
        }
        Some(root) => {
            if parsed.has_errors() {
                ui.label(
                    RichText::new("Showing what could be read; the text has problems.")
                        .small()
                        .color(theme::WARN),
                );
            }
            node(ui, id, root, src, None, 0);
        }
    }
}

fn node(ui: &mut Ui, id: Id, n: &Node, src: &str, label: Option<&str>, depth: usize) {
    let mono = TextStyle::Monospace.resolve(ui.style());
    let children: Option<Vec<(Option<String>, &Node)>> = match &n.kind {
        NodeKind::List(items) | NodeKind::Vector(items) => Some(
            items
                .iter()
                .enumerate()
                .map(|(i, c)| (Some(i.to_string()), c))
                .collect(),
        ),
        NodeKind::Set(items) => Some(items.iter().map(|c| (None, c)).collect()),
        NodeKind::Map(entries) => Some(
            entries
                .iter()
                .flat_map(|(k, v)| {
                    if is_compact(k) {
                        vec![(Some(abbreviate(k.text(src), 48)), v)]
                    } else {
                        // Composite keys are shown as their own entries.
                        vec![(Some("key".to_owned()), k), (Some("value".to_owned()), v)]
                    }
                })
                .collect(),
        ),
        _ => None,
    };
    let Some(children) = children else {
        ui.horizontal_wrapped(|ui| {
            if let Some(label) = label {
                ui.label(RichText::new(label).font(mono.clone()).color(theme::MUTED));
            }
            let text = n.text(src);
            let shown = abbreviate(text, 160);
            let r = ui.label(RichText::new(&shown).font(mono).color(scalar_color(n)));
            if shown != text {
                r.on_hover_text(abbreviate(text, 4000));
            }
        });
        return;
    };
    let mut header = egui::text::LayoutJob::default();
    if let Some(label) = label {
        header.append(
            &format!("{label}  "),
            0.0,
            TextFormat::simple(mono.clone(), theme::MUTED),
        );
    }
    header.append(&n.summary(), 0.0, TextFormat::simple(mono, theme::INK));
    let len = children.len();
    egui::CollapsingHeader::new(header)
        .id_salt(id)
        // The root starts open; children are paged, so even large values stay cheap to show.
        .default_open(depth == 0)
        .show(ui, |ui| {
            let shown_id = id.with("shown");
            let shown = ui.data(|d| d.get_temp::<usize>(shown_id)).unwrap_or(PAGE);
            for (i, (label, child)) in children.iter().take(shown).enumerate() {
                node(ui, id.with(i), child, src, label.as_deref(), depth + 1);
            }
            if len > shown {
                let next = PAGE.min(len - shown);
                if ui
                    .button(format!("Show {next} more ({} not shown)", len - shown))
                    .clicked()
                {
                    ui.data_mut(|d| d.insert_temp(shown_id, shown + next));
                }
            }
        });
}

fn is_compact(n: &Node) -> bool {
    n.child_count().is_none()
}

fn scalar_color(n: &Node) -> Color32 {
    match n.kind {
        NodeKind::Nil | NodeKind::Boolean(_) => syntax::LITERAL,
        NodeKind::Integer | NodeKind::Float | NodeKind::Ratio => syntax::NUMBER,
        NodeKind::String(_) | NodeKind::Char(_) => syntax::STRING,
        NodeKind::Keyword => syntax::KEYWORD,
        NodeKind::Tagged { .. } => syntax::TAG,
        _ => syntax::SYMBOL,
    }
}

/// One line of at most `max` characters.
pub fn abbreviate(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut last_space = false;
    for c in s.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c == ' ' && last_space {
            continue;
        }
        last_space = c == ' ';
        if count + 1 >= max {
            out.push('…');
            return out;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// Read-only, highlighted, selectable source code.
pub fn code_view(ui: &mut Ui, id: Id, source: &str) {
    let font = TextStyle::Monospace.resolve(ui.style());
    // Code keeps its line structure; long lines scroll horizontally instead of wrapping.
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let job = highlight_job(buf.as_str(), font.clone(), &[], f32::INFINITY);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let mut text = source;
    egui::ScrollArea::horizontal()
        .id_salt(id.with("scroll"))
        .show(ui, |ui| {
            egui::TextEdit::multiline(&mut text)
                .id(id)
                .code_editor()
                .lock_focus(false)
                .desired_width(f32::INFINITY)
                .layouter(&mut layouter)
                .show(ui);
        });
}

/// A large editor with the inspector beside it, shown in a modal. Returns whether the text
/// changed and whether the modal should close.
pub fn expanded_editor(
    ctx: &egui::Context,
    id: Id,
    title: &str,
    text: &mut String,
    cache: &mut EdnCache,
) -> (bool, bool) {
    let mut changed = false;
    let mut close = false;
    let screen = ctx.content_rect();
    let response = egui::Modal::new(id.with("modal")).show(ctx, |ui| {
        ui.set_width((screen.width() - 80.0).max(400.0));
        let height = (screen.height() - 180.0).max(240.0);
        ui.horizontal(|ui| {
            ui.heading(title);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Done").on_hover_text("Close (Esc)").clicked() {
                    close = true;
                }
            });
        });
        ui.add_space(6.0);
        let parsed = cache.parsed(id, text);
        let size = egui::vec2(ui.available_width(), height + 24.0);
        ui.allocate_ui(size, |ui| {
            ui.columns(2, |cols| {
                cols[0].label(RichText::new("Text").small().color(theme::MUTED));
                changed = editor_sized(&mut cols[0], id, "EDN value", text, &parsed, height, true);
                cols[1].label(RichText::new("Inspector").small().color(theme::MUTED));
                egui::Frame::new()
                    .fill(theme::SUNKEN)
                    .stroke(theme::hairline())
                    .corner_radius(theme::RADIUS)
                    .inner_margin(6.0)
                    .show(&mut cols[1], |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt(id.with("modal-tree"))
                            .max_height(height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| inspector(ui, id.with("modal-tree-root"), &parsed, text));
                    });
            })
        });
        diagnostics(ui, id, text, &parsed);
    });
    (changed, close || response.should_close())
}
