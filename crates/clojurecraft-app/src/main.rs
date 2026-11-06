//! ClojureCraft: create, test, and optimize small pure Clojure data transformations with a local
//! coding model.

mod app;
mod editor;
mod edn_view;
mod implementation;
mod jobs;
mod keys;
mod library;
mod session;
mod theme;
mod views;

const APP_ID: &str = "org.clojurecraft.ClojureCraft";

/// Below this size the three-column workspace stops being usable.
pub const MIN_WINDOW: [f32; 2] = [1024.0, 640.0];

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let viewport = egui::ViewportBuilder::default()
        .with_title("ClojureCraft")
        .with_app_id(APP_ID)
        .with_inner_size([1360.0, 860.0])
        .with_min_inner_size(MIN_WINDOW);
    let options = eframe::NativeOptions {
        viewport,
        persist_window: true,
        // Kept with the rest of the application data (and its override) rather than in a
        // separate per-toolkit location.
        persistence_path: clojurecraft_runtime::DataDirs::discover().map(|d| d.ui_state()),
        ..Default::default()
    };
    eframe::run_native(
        "ClojureCraft",
        options,
        Box::new(|cc| Ok(Box::new(app::ClojureCraftApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("could not open the application window: {e}"))
}
