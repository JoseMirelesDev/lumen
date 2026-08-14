//! Per-domain UI controllers. Each owns one slice of the AppWindow contract
//! (auth, shell/nav, chat) and receives a shared `on_changed` closure from
//! the orchestrating UiController (controller.rs) — calling it re-pushes the
//! whole UI state. Voice stays in its own VoiceController (src/voice.rs);
//! the orchestrator wires it and the two model-reflect callbacks.

pub mod auth;
pub mod chat;
pub mod shell;
