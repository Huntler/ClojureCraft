//! ClojureCraft: create, test, and optimize small pure Clojure data transformations with a local
//! coding model.

mod app;
mod contract_view;
mod dataset_view;
mod describe_view;
mod edge_cases_view;
mod editor;
mod edn_view;
mod execution;
mod explain_view;
mod generation_view;
mod implementation;
mod inference;
mod interpretation_view;
mod jobs;
mod keys;
mod knowledge;
mod knowledge_view;
mod library;
mod model;
mod model_view;
mod models_view;
mod project;
mod project_view;
mod reference_view;
mod references;
mod review;
mod review_view;
mod session;
mod setup_view;
mod step_inspection;
mod step_preview_view;
mod theme;
mod validation_view;
mod views;

const APP_ID: &str = "org.clojurecraft.ClojureCraft";

/// Below this size the three-column workspace stops being usable.
pub const MIN_WINDOW: [f32; 2] = [1024.0, 640.0];

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        return mcp_bridge();
    }
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

/// `clojurecraft mcp`: the stdio server an MCP client starts. It relays to the running
/// application, starting it first when needed (unless `CLOJURECRAFT_MCP_NO_LAUNCH` is set).
/// stdout carries only protocol messages, so logs go to stderr.
fn mcp_bridge() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let dirs = clojurecraft_runtime::DataDirs::discover()
        .ok_or_else(|| anyhow::anyhow!("no data directory is available for this user"))?;
    let launch = || -> std::io::Result<()> {
        tracing::info!("starting ClojureCraft for an MCP client");
        std::process::Command::new(std::env::current_exe()?)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(drop)
    };
    let may_launch = std::env::var_os("CLOJURECRAFT_MCP_NO_LAUNCH").is_none();
    clojurecraft_mcp::bridge::run(
        &dirs.mcp_socket(),
        may_launch.then_some(&launch as &dyn Fn() -> std::io::Result<()>),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
