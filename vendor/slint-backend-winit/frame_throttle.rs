// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::rc::{Rc, Weak};

use i_slint_core::timers::{Timer, TimerMode};

#[cfg(target_vendor = "apple")]
mod apple_display_link;

use crate::winitwindowadapter::WinitWindowAdapter;

pub fn create_frame_throttle(
    window_adapter: Weak<WinitWindowAdapter>,
    _winit_window: &winit::window::Window,
    _is_wayland: bool,
) -> Box<dyn FrameThrottle> {
    if _is_wayland {
        WinitBasedFrameThrottle::create()
    } else {
        #[cfg(target_vendor = "apple")]
        if let Some(throttle) =
            apple_display_link::try_create(window_adapter.clone(), _winit_window)
        {
            return throttle;
        }
        TimerBasedFrameThrottle::create(window_adapter)
    }
}

pub trait FrameThrottle {
    fn request_throttled_redraw(&self, winit_window: &winit::window::Window);
}

struct TimerBasedFrameThrottle {
    window_adapter: Weak<WinitWindowAdapter>,
    timer: Rc<Timer>,
}

impl TimerBasedFrameThrottle {
    fn create(window_adapter: Weak<WinitWindowAdapter>) -> Box<dyn FrameThrottle> {
        Box::new(Self { window_adapter, timer: Rc::new(Timer::default()) })
    }
}

impl FrameThrottle for TimerBasedFrameThrottle {
    fn request_throttled_redraw(&self, winit_window: &winit::window::Window) {
        if self.timer.running() {
            return;
        }
        let refresh_interval_millihertz = winit_window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .unwrap_or(60000) as u64;
        let window_adapter = self.window_adapter.clone();
        let timer = Rc::downgrade(&self.timer);
        let interval =
            std::time::Duration::from_millis((1000 * 1000) / refresh_interval_millihertz);
        self.timer.start(TimerMode::Repeated, interval, move || {
            // Lumen fork fix: check the pending flag BEFORE requesting the
            // redraw, and stop as soon as the request was served. The original
            // implementation called `redraw_now` unconditionally and checked
            // `pending_redraw()` afterwards, so the final timer tick queued a
            // RedrawRequested with no pending redraw left — every
            // `request_redraw()` produced TWO renders (the second empty, but
            // still a full traversal + present). This caused the animated
            // campfire tick (12.5 Hz) to render at 25 Hz on X11.
            let Some(window_adapter) = window_adapter.upgrade() else { return };
            let keep_running = window_adapter.pending_redraw();

            if !keep_running {
                if let Some(timer) = timer.upgrade() {
                    timer.stop();
                }
                return;
            }

            if let Some(winit_window) = window_adapter.winit_window() {
                winit_window.request_redraw();
            }
        });
    }
}


struct WinitBasedFrameThrottle;

impl WinitBasedFrameThrottle {
    fn create() -> Box<dyn FrameThrottle> {
        Box::new(Self)
    }
}

impl FrameThrottle for WinitBasedFrameThrottle {
    fn request_throttled_redraw(&self, winit_window: &winit::window::Window) {
        winit_window.request_redraw();
    }
}
