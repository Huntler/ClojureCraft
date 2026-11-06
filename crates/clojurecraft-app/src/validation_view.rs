//! Validation results: the checks of a report with their diagnostics, expected and actual
//! values, timing, and whether the report still describes the current function.

use clojurecraft_domain::{
    Check, CheckStatus, CheckSubject, Component, Freshness, FunctionDocument, Timestamp, ValidationReport,
};
use clojurecraft_workflow::assistant::Attempt;
use clojurecraft_workflow::spec;
use clojurecraft_workflow::validation;
use egui::{Id, RichText, Ui};

use crate::edn_view;
use crate::theme;

/// How the function's examples and tests name a check.
pub fn subject_label(doc: &FunctionDocument, subject: &CheckSubject) -> String {
    match subject {
        CheckSubject::Parse => "Reading the code".into(),
        CheckSubject::Structure => "Structure and dependencies".into(),
        CheckSubject::Lint => "Lint".into(),
        CheckSubject::Load => "Loading".into(),
        CheckSubject::Contract => "Output contract".into(),
        CheckSubject::Example(id) => doc.examples.iter().position(|e| e.id == *id).map_or_else(
            || "A removed example".into(),
            |i| spec::example_label(i, &doc.examples[i]),
        ),
        CheckSubject::Test(id) => doc
            .tests
            .iter()
            .filter(|t| t.is_active())
            .enumerate()
            .find(|(_, t)| t.id == *id)
            .map_or_else(|| "A removed test".into(), |(n, t)| spec::test_label(n, t)),
    }
}

pub fn status_text(status: CheckStatus) -> (&'static str, egui::Color32) {
    match status {
        CheckStatus::Passed => ("passed", theme::ACCENT_INK),
        CheckStatus::Failed => ("failed", theme::BAD),
        CheckStatus::Error => ("error", theme::BAD),
        CheckStatus::Timeout => ("timed out", theme::BAD),
        CheckStatus::ResourceLimit => ("resource limit", theme::BAD),
        CheckStatus::Forbidden => ("forbidden", theme::BAD),
        CheckStatus::Skipped => ("not run", theme::MUTED),
    }
}

/// "3 of 5 checks passed", or "all 5 checks passed".
pub fn summary(report: &ValidationReport) -> String {
    let t = validation::tally(report);
    let total = report.checks.len();
    if report.passed() {
        format!("All {total} checks passed")
    } else if t.skipped > 0 {
        format!(
            "{} of {total} checks passed, {} failed, {} not run",
            t.passed, t.failed, t.skipped
        )
    } else {
        format!("{} of {total} checks passed, {} failed", t.passed, t.failed)
    }
}

pub fn ago(t: Timestamp) -> String {
    let secs = (Timestamp::now().0 - t.0).max(0) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

fn component(c: Component) -> &'static str {
    match c {
        Component::Specification => "the specification",
        Component::Code => "the code",
        Component::Tests => "the accepted tests",
        Component::BenchmarkConfiguration => "the benchmark configuration",
    }
}

/// A banner saying the report no longer describes the function, if so.
pub fn freshness_banner(ui: &mut Ui, freshness: &Freshness) {
    if let Freshness::Stale(changed) = freshness {
        let what: Vec<&str> = changed.iter().map(|c| component(*c)).collect();
        ui.label(
            RichText::new(format!(
                "Outdated: {} changed since these checks ran. They are kept for inspection and are \
                 not a current result.",
                what.join(" and ")
            ))
            .small()
            .color(theme::WARN),
        );
    }
}

fn value_box(ui: &mut Ui, id: Id, label: &str, text: &str) {
    ui.label(RichText::new(label).small().color(theme::MUTED));
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(4.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt(id)
                .max_height(160.0)
                .show(ui, |ui| edn_view::code_view(ui, id, text));
        });
}

fn check_row(ui: &mut Ui, doc: &FunctionDocument, report: &ValidationReport, c: &Check, i: usize) {
    let (status, color) = status_text(c.status);
    let label = subject_label(doc, &c.subject);
    let has_detail = !c.diagnostics.is_empty() || c.expected.is_some() || c.actual.is_some();
    let head = |ui: &mut Ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(status).color(color).strong());
            ui.label(&label);
            if let Some(ms) = c.duration_ms {
                ui.label(RichText::new(format!("{ms} ms")).small().color(theme::MUTED));
            }
        });
    };
    if !has_detail {
        head(ui);
        return;
    }
    let failing = !matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped);
    let short: String = if label.chars().count() > 48 {
        label.chars().take(47).chain(std::iter::once('…')).collect()
    } else {
        label.clone()
    };
    egui::CollapsingHeader::new(RichText::new(format!("{status} · {short}")).color(color))
        .id_salt(("check", report.id, i))
        .default_open(failing)
        .show(ui, |ui| {
            if let Some(ms) = c.duration_ms {
                ui.label(RichText::new(format!("{ms} ms")).small().color(theme::MUTED));
            }
            for d in &c.diagnostics {
                let warning = d.contains("warning:") || d.starts_with("Printed:");
                ui.label(RichText::new(d).small().color(if warning || !failing {
                    theme::MUTED
                } else {
                    theme::INK
                }));
            }
            if let Some(e) = &c.expected {
                value_box(ui, Id::unique(("expected", report.id, i)), "Expected", e.as_str());
            }
            if let Some(a) = &c.actual {
                value_box(ui, Id::unique(("actual", report.id, i)), "Actual", a.as_str());
            }
        });
}

/// Every check of a report; failing ones open.
pub fn checks(ui: &mut Ui, doc: &FunctionDocument, report: &ValidationReport) {
    for (i, c) in report.checks.iter().enumerate() {
        check_row(ui, doc, report, c, i);
    }
    let env = &report.environment;
    let mut details = vec![format!("Clojure {}", env.clojure_version)];
    if !env.java_version.is_empty() {
        details.push(env.java_version.clone());
    }
    if !env.platform.is_empty() {
        details.push(env.platform.clone());
    }
    if let Some(s) = &report.settings {
        details.push(format!(
            "{} s per call · floats within {:e}{}",
            s.call_timeout_ms / 1000,
            s.float_tolerance,
            if s.unordered_output {
                " · output order ignored"
            } else {
                ""
            }
        ));
    }
    ui.label(RichText::new(details.join(" · ")).small().color(theme::MUTED));
}

/// A compact list of a generation's attempts with their checks and code.
pub fn attempts(ui: &mut Ui, doc: &FunctionDocument, attempts: &[Attempt], salt: &str) {
    for a in attempts {
        // Headers stay short: a long, unwrapped header would widen the column.
        let (text, color) = match &a.report {
            Some(r) if r.passed() => (summary(r), theme::ACCENT_INK),
            Some(r) => (summary(r), theme::BAD),
            None if a.code.is_none() => ("no usable code".to_owned(), theme::WARN),
            None => ("not checked".to_owned(), theme::WARN),
        };
        egui::CollapsingHeader::new(RichText::new(format!("Attempt {}: {text}", a.number)).color(color))
            .id_salt(("attempt", salt, doc.id, a.number))
            .show(ui, |ui| {
                if let Some(n) = &a.note {
                    ui.label(RichText::new(n).small().color(theme::MUTED));
                }
                if let Some(r) = &a.report {
                    checks(ui, doc, r);
                }
                if let Some(code) = &a.code {
                    ui.collapsing("Code", |ui| {
                        egui::Frame::new()
                            .fill(theme::SUNKEN)
                            .stroke(theme::hairline())
                            .corner_radius(theme::RADIUS)
                            .inner_margin(6.0)
                            .show(ui, |ui| {
                                edn_view::code_view(
                                    ui,
                                    Id::unique(("attempt-code", salt, doc.id, a.number)),
                                    code,
                                );
                            });
                    });
                }
            });
    }
}
