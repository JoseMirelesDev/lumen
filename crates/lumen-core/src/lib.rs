//! Lumen app core — framework-agnostic (no Tauri, no Slint, no UI).
//!
//! Layers (one-way dependency): protocol → api/settings → auth/state/event.
//! A host (Slint app, tests) wires [`AppServices`] and drives the actions;
//! observable changes flow through the [`EventBus`].

pub mod api;
pub mod auth;
pub mod event;
pub mod presence;
pub mod protocol;
pub mod settings;
pub mod state;

pub use api::{ApiClient, ApiError};
pub use auth::AuthService;
pub use event::{CoreEvent, EventBus};
pub use presence::{PresenceClient, PresenceOut};
pub use protocol::*;
pub use settings::Settings;
pub use state::{is_online, now_iso, ShellState, View};
