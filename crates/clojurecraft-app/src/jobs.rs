//! Background work. Jobs run on their own threads and report typed events; the UI drains the
//! channel each frame and never blocks on a job.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use clojurecraft_hardware::{HardwareReport, MemoryBudget, default_budget};
use clojurecraft_runtime::{DataDirs, Manifest, components, platform_key};

/// Which runtime components are installed.
#[derive(Clone, Debug, Default)]
pub struct RuntimeStatus {
    pub data_dir: Option<String>,
    pub llama_cpp: Option<String>,
    pub java: bool,
    pub clojure: bool,
}

#[derive(Debug)]
pub enum Event {
    SystemChecked {
        report: Box<HardwareReport>,
        budget: MemoryBudget,
        runtime: RuntimeStatus,
    },
}

pub struct Jobs {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    ctx: egui::Context,
}

impl Jobs {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        Self { tx, rx, ctx }
    }

    pub fn drain(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// Detects hardware and installed runtimes.
    pub fn check_system(&self) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let report = clojurecraft_hardware::detect();
            let budget = default_budget(&report, report.recommended_backend());
            let mut runtime = RuntimeStatus::default();
            if let (Ok(manifest), Some(dirs)) = (Manifest::bundled(), DataDirs::discover()) {
                runtime.data_dir = Some(dirs.root().display().to_string());
                runtime.llama_cpp = components::llama_server(&manifest, &dirs).map(|(i, _)| i.version);
                runtime.java = components::java(&manifest, &dirs, &platform_key()).is_some();
                runtime.clojure = components::clojure_jars(&manifest, &dirs).is_some();
            }
            let _ = tx.send(Event::SystemChecked {
                report: Box::new(report),
                budget,
                runtime,
            });
            ctx.request_repaint();
        });
    }
}
