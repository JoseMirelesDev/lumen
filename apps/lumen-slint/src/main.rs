// Lumen desktop client (Slint host) — entry point.
// Boots lumen-core services + the VoiceController, attaches the UiController
// to the AppWindow and runs the Slint event loop. All async work runs on the
// tokio runtime; UI updates only via Weak<AppWindow>::upgrade_in_event_loop.

// Windows: link with the GUI subsystem in release builds so no console window
// opens next to the app. Debug builds keep the console (panics/backtraces stay
// visible when running `cargo run`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod controller;
mod model;
mod voice;

pub use model::*; // re-export AppWindow + the generated Slint structs

use std::sync::Arc;

use lumen_core::{ApiClient, AuthService, EventBus, Settings, ShellState};
use slint::ComponentHandle;

use crate::controller::UiController;
use crate::voice::VoiceController;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let ui = AppWindow::new()?;

    let bus = EventBus::new();
    let settings = Arc::new(Settings::load());
    let api = Arc::new(ApiClient::new(settings.backend_url()));
    let auth = AuthService::new(api.clone(), settings.clone(), bus.clone());
    let shell = ShellState::new(api.clone(), bus.clone());
    let voice = VoiceController::new(api.clone(), settings.clone(), rt.handle().clone());
    let ctrl = UiController::new(api, auth, shell, voice, bus, rt.handle().clone());
    ctrl.attach(&ui);

    ui.run()?;
    Ok(())
}
