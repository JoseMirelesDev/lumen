//! Dev/test harness for the partial-rendering fork (LUMEN_RENDER_TEST).
//!
//! Drives the campfire ticks and a scripted scroll of the Settings ScrollView
//! without a live voice connection, and dumps PPM snapshots to /tmp/lumen-test/
//! so ghost pixels / stale regions are visible programmatically. Run with:
//!   LUMEN_RENDER_TEST=1 SLINT_SKIA_PARTIAL_RENDERING=log target/release/lumen
//!
//! This is test scaffolding only; it is inert unless the env var is set.

use slint::{ComponentHandle, LogicalPosition, Timer, TimerMode};
use std::time::{Duration, Instant};

use crate::{particles, AppWindow};

fn save_ppm(ui: &AppWindow, path: &str) {
    match ui.window().take_snapshot() {
        Ok(buf) => {
            let w = buf.width();
            let h = buf.height();
            let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in buf.as_bytes().chunks_exact(4) {
                ppm.extend_from_slice(&px[..3]);
            }
            if let Err(e) = std::fs::write(path, &ppm) {
                eprintln!("[rendertest] snapshot write failed: {e}");
            } else {
                eprintln!("[rendertest] snapshot {path} ({w}x{h})");
            }
        }
        Err(e) => eprintln!("[rendertest] snapshot failed: {e}"),
    }
}

pub fn maybe_run(ui: &AppWindow) {
    if std::env::var_os("LUMEN_RENDER_TEST").is_none() {
        return;
    }

    // Force the app into the "connected, particles on, settings open" state.
    ui.set_logged_in(true);
    ui.set_voice_visible(true);
    ui.set_voice_active(true);
    ui.set_voice_particles_enabled(true);
    ui.set_voice_animations_enabled(true);
    ui.set_reduced_motion(false);
    ui.set_voice_channel_name("test-channel".into());
    ui.set_voice_status("connected — 0 peer(s)".into());
    ui.set_voice_local_user("Tester".into());
    ui.set_overlay("settings".into());

    let size = ui.window().size();
    eprintln!(
        "[rendertest] window {}x{} (pre-run size; wheel pos computed lazily)",
        size.width, size.height
    );

    let dir = "/tmp/lumen-test";
    let _ = std::fs::create_dir_all(dir);

    let start = Instant::now();
    let ms = |start: &Instant| start.elapsed().as_millis();

    // Fire ticks at ~12.5 Hz, like the voice push. Drives the skip-render path.
    let weak = ui.as_weak();
    let fire_timer: &'static Timer = Box::leak(Box::new(Timer::default()));
    fire_timer.start(TimerMode::Repeated, Duration::from_millis(80), move || {
        if let Some(ui) = weak.upgrade() {
            particles::tick_fire(&ui.window());
        }
    });

    // Scroll phase + periodic snapshots.
    let weak = ui.as_weak();
    let driver: &'static Timer = Box::leak(Box::new(Timer::default()));
    let wheel_pos = std::cell::Cell::new(LogicalPosition::new(0.0, 0.0));
    let wheel_ready = std::cell::Cell::new(false);
    driver.start(TimerMode::Repeated, Duration::from_millis(60), move || {
        let Some(ui) = weak.upgrade() else { return };
        let t = ms(&start);

        // The window is resized to its saved size shortly after show; compute
        // the settings-card position from the CURRENT size each scroll tick.
        if (2500..=9000).contains(&t) {
            let size = ui.window().size();
            if size.width > 0 && size.height > 0 {
                // Card is centered, width min(420, w-24), height min(480, h-24).
                let card_w = 420.0f32.min(size.width as f32 - 24.0);
                let card_h = 480.0f32.min(size.height as f32 - 24.0);
                let cx = size.width as f32 / 2.0;
                let cy = size.height as f32 / 2.0;
                if !wheel_ready.get() {
                    eprintln!(
                        "[rendertest] window {}x{} card {card_w}x{card_h}",
                        size.width, size.height
                    );
                    wheel_ready.set(true);
                }
                // Inside the card, below the header row, over the ScrollView.
                wheel_pos.set(LogicalPosition::new(cx, cy - card_h / 4.0));
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                    position: wheel_pos.get(),
                    delta_x: 0.0,
                    delta_y: 40.0,
                });
                ui.window().request_redraw();
            }
        }

        if t % 500 < 60 {
            let phase = if t < 2500 { "fire" } else { "scroll" };
            save_ppm(&ui, &format!("{dir}/{phase}-{t:05}.ppm"));
        }

        if t > 9500 {
            save_ppm(&ui, &format!("{dir}/final-{t:05}.ppm"));
            eprintln!("[rendertest] done at {t}ms");
            std::process::exit(0);
        }
    });
}
