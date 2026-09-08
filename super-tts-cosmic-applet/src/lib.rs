// SPDX-License-Identifier: GPL-3.0-only
mod app;
mod config;
mod daemon;
mod models;
mod ui;
mod util;

// Types needed by the binary entry point (`main.rs`).
pub use app::SuperTtsApplet;
pub use models::theme::VisualizationSide;

// Crate version/repository sourced from Cargo.toml for UI display and
// CLI metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Run the Super TTS COSMIC applet.
///
/// # Errors
///
/// Returns an error if the applet fails to start or encounters a
/// runtime error during execution.
pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<SuperTtsApplet>(VisualizationSide::Full)
}
