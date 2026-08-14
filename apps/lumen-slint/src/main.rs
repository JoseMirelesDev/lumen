// Lumen desktop client (Slint host) — entry point.
// Boots lumen-core services + the VoiceController, attaches the UiController
// to the AppWindow and runs the Slint event loop. All async work runs on the
// tokio runtime; UI updates only via Weak<AppWindow>::upgrade_in_event_loop.

// Windows: link with the GUI subsystem in release builds so no console window
// opens next to the app. Debug builds keep the console (panics/backtraces stay
// visible when running `cargo run`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod controller;
mod ctrl;
mod model;
mod particles;
mod rendertest;
mod sound;
mod voice;

pub use model::*; // re-export AppWindow + the generated Slint structs

use std::sync::Arc;

use lumen_core::{ApiClient, AuthService, EventBus, Settings, ShellState};
use slint::ComponentHandle;

use crate::controller::UiController;
use crate::voice::VoiceController;

/// OAuth / invite deeplinks (Fase 4, ARCHITECTURE.md §9):
///   lumen://auth/callback?token=…&refreshToken=…
///   lumen://invite/CODE
pub enum Deeplink {
    AuthCallback { token: String, refreshToken: Option<String> },
    Invite(String),
}

fn parse_deeplink(args: &[String]) -> Option<Deeplink> {
    let url = args.iter().find(|a| a.starts_with("lumen://"))?;
    let rest = url.strip_prefix("lumen://")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let params: std::collections::HashMap<String, String> = query
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), urlencoding_decode(v)))
        })
        .collect();
    match path {
        "auth/callback" => Some(Deeplink::AuthCallback {
            token: params.get("token")?.clone(),
            refreshToken: params.get("refreshToken").cloned(),
        }),
        // lumen://invite/CODE (path form) or lumen://invite?code=CODE
        "invite" => params.get("code").map(|c| Deeplink::Invite(c.clone())),
        _ if path.starts_with("invite/") => {
            let code = path.trim_start_matches("invite/");
            if code.is_empty() { None } else { Some(Deeplink::Invite(code.to_string())) }
        }
        _ => None,
    }
}

/// Percent-decode a query value (no extra dependency; '+' → space, %XX → byte).
fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Dev helper: read the `sub` claim from a JWT without validating it
/// (base64url payload decode, RFC 4648 §5, no padding). `None` on any
/// malformed input. Only used by the LUMEN_AUTOJOIN hook.
fn jwt_sub(token: &str) -> Option<String> {
    let b64 = token.split('.').nth(1)?;
    let mut out = Vec::with_capacity(b64.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in b64.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => continue,
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    serde_json::from_slice::<serde_json::Value>(&out)
        .ok()?
        .get("sub")?
        .as_str()
        .map(|s| s.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Skia partial rendering: only the dirty region of the frame is
    // redrawn, instead of re-rendering the whole scene on every animation
    // tick. Without it, the animated campfire (5 fps fire-frame timer)
    // forces a full-scene redraw — layout + upload + compose of every
    // gradient/seat/HUD — which pins a core at ~20-25% CPU even though the
    // GPU does the drawing. Read at renderer creation, so set it before the
    // first window is created. Respect an explicit user override.
    if std::env::var_os("SLINT_SKIA_PARTIAL_RENDERING").is_none() {
        std::env::set_var("SLINT_SKIA_PARTIAL_RENDERING", "1");
    }

    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let ui = AppWindow::new()?;

    let bus = EventBus::new();
    let settings = Arc::new(Settings::load());
    let api = Arc::new(ApiClient::new(settings.backend_url()));
    let auth = AuthService::new(api.clone(), settings.clone(), bus.clone());
    let shell = ShellState::new(api.clone(), bus.clone());
    let sfx = Arc::new(sound::Sfx::new());
    let voice = VoiceController::new(api.clone(), settings.clone(), rt.handle().clone(), sfx.clone(), bus.clone());

    // Dev hook: auto-join a voice channel at startup (no UI interaction).
    // Reads the persisted token from settings; the user id comes from the JWT
    // `sub` claim. Used by the audio-CPU measurement harness to get the app
    // into the connected state reproducibly.
    if let Some(channel_id) = std::env::var_os("LUMEN_AUTOJOIN") {
        let channel_id = channel_id.to_string_lossy().to_string();
        let channel_name =
            std::env::var("LUMEN_AUTOJOIN_NAME").unwrap_or_else(|_| "auto-join".to_string());
        let voice = voice.clone();
        let settings = settings.clone();
        rt.spawn(async move {
            // Give the window/login a beat to settle before joining.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Some(token) = settings.token() else {
                eprintln!("lumen: LUMEN_AUTOJOIN set but no token in settings");
                return;
            };
            // Decode the JWT payload (unvalidated) to read the `sub` claim.
            let user_id = jwt_sub(&token);
            let Some(user_id) = user_id else {
                eprintln!("lumen: LUMEN_AUTOJOIN: no sub claim in token");
                return;
            };
            eprintln!("lumen: LUMEN_AUTOJOIN joining channel {channel_id} as {user_id}");
            voice
                .join(
                    settings.backend_url(),
                    token,
                    user_id,
                    "dev".to_string(),
                    channel_id,
                    channel_name,
                )
                .await;
        });
    }

    let ctrl = UiController::new(api, auth, shell, voice, bus, rt.handle().clone(), sfx);
    ctrl.attach(&ui);

    // OS reduced-motion: read it ONCE on a background thread (gsettings can
    // hang without a session bus — never block the UI thread on it).
    controller::warm_os_reduced_motion();

    // OAuth deeplink (Fase 4): the OS re-launches the app with
    // `lumen://auth/callback?token=…` after the browser flow. If the app was
    // already running, this lands in a second instance (single-instance IPC
    // is documented as future work — the browser-external flow covers it).
    let args: Vec<String> = std::env::args().collect();
    if let Some(link) = parse_deeplink(&args) {
        match link {
            Deeplink::AuthCallback { token, refreshToken } => {
                eprintln!("lumen: deeplink auth/callback — restoring session");
                ctrl.auth.restore_from_deeplink(token, refreshToken);
            }
            Deeplink::Invite(code) => {
                eprintln!("lumen: deeplink invite {code} — (join dialog prefill: Fase 4)");
            }
        }
    }

    // Partículas del campfire: frames generados en RAM (Rust) rotados cada tick
    // desde el push de voz (sin timer propio, sin notifier). El registro es
    // incondicional; la visibilidad/avance se gatea por el toggle de Settings
    // (partículas de la fogata) y por reduced-motion.
    particles::init(&ui);

    // Dev/test harness: drives the fire ticks + a scripted scroll of the
    // Settings ScrollView + snapshots, to verify partial-rendering behavior
    // without a live voice connection. Run with LUMEN_RENDER_TEST=1 and
    // SLINT_SKIA_PARTIAL_RENDERING=log to observe repaint patterns.
    crate::rendertest::maybe_run(&ui);

    ui.run().map_err(|e| {
        // The winit loop exits with code 1 and NO message when a render/GL
        // error is set as `loop_error` — make the failure diagnosable (see
        // the audio reconnect churn that kills the app intermittently).
        eprintln!("event loop error: {e:?}");
        e
    })?;
    Ok(())
}

#[cfg(test)]
mod deeplink_tests {
    use super::{parse_deeplink, urlencoding_decode, Deeplink};

    #[test]
    fn parses_auth_callback() {
        let args = vec!["lumen".to_string(), "lumen://auth/callback?token=abc.def&refreshToken=xyz".to_string()];
        match parse_deeplink(&args) {
            Some(Deeplink::AuthCallback { token, refreshToken }) => {
                assert_eq!(token, "abc.def");
                assert_eq!(refreshToken.as_deref(), Some("xyz"));
            }
            _ => panic!("wrong deeplink"),
        }
    }

    #[test]
    fn parses_invite() {
        let args = vec!["lumen://invite/CODE123".to_string()];
        match parse_deeplink(&args) {
            Some(Deeplink::Invite(code)) => assert_eq!(code, "CODE123"),
            _ => panic!("wrong deeplink"),
        }
    }

    #[test]
    fn ignores_other_args() {
        let args = vec!["--flag".to_string(), "lumen://whatever".to_string()];
        assert!(parse_deeplink(&args).is_none());
    }

    #[test]
    fn decodes_percent_and_plus() {
        assert_eq!(urlencoding_decode("a+b%20c"), "a b c");
        assert_eq!(urlencoding_decode("%2F"), "/");
    }
}
