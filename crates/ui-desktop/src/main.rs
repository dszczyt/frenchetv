use anyhow::Result;
use tracing_subscriber::EnvFilter;

mod app;
pub(crate) mod drm;
mod player;
mod screens;
pub(crate) mod theme;
pub(crate) mod widevine;
pub(crate) mod widgets;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("frenchetv")
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "frenchetv",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
