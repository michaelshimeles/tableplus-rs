//! tableplus-rs: a fast, native database GUI.
//!
//! Environment:
//! - `TABLEPLUS_RS_HOME` — config directory override (connections, history, settings, window state)
//! - `TABLEPLUS_RENDERER=glow|wgpu` — force a renderer (default: wgpu on macOS, glow elsewhere)
//! - `RUST_LOG` — log filter

mod app;
mod conn_dialog;
mod editor;
mod export_dialog;
mod grid;
mod icons;
#[cfg(target_os = "macos")]
mod macos_menu;
mod plan_flame;
mod plan_graph;
mod plan_view;
mod query_tab;
mod quick_open;
mod session;
mod settings;
mod sidebar;
mod staged;
mod structure;
mod table_tab;
mod task;
mod theme;
mod util;
mod welcome;
mod widgets;
mod workspace;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,tableplus=info".into()),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("tableplus-io")
        .enable_all()
        .build()
        .expect("tokio runtime");

    let renderer = match std::env::var("TABLEPLUS_RENDERER").as_deref() {
        Ok("wgpu") => eframe::Renderer::Wgpu,
        Ok("glow") => eframe::Renderer::Glow,
        _ if cfg!(target_os = "macos") => eframe::Renderer::Wgpu,
        _ => eframe::Renderer::Glow,
    };

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("tableplus-rs")
        .with_app_id("tableplus-rs")
        .with_inner_size([1360.0, 860.0])
        .with_min_inner_size([760.0, 480.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    if cfg!(target_os = "macos") {
        // Unified toolbar like native macOS apps: content runs under a transparent titlebar.
        viewport = viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false);
    }

    let mut options = eframe::NativeOptions { viewport, renderer, centered: true, ..Default::default() };
    if std::env::var_os("TABLEPLUS_RS_HOME").is_some() {
        options.persistence_path = Some(tableplus_core::store::config_dir().join("app.ron"));
    }
    eframe::run_native("tableplus-rs", options, Box::new(|cc| Ok(Box::new(app::TablePlusApp::new(cc, runtime)))))
}
