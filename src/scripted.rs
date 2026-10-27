//! Scripted input for headless testing, from the environment:
//! - `XP_CLICKS=<secs>:<x>:<y>[:r],...`: clicks at desktop coordinates (left, or right with `:r`);
//! - `XP_KEYS=<secs>:<virtual key>[+<virtual key>...],...`: key presses (27 is Esc, 17+69 is
//!   Ctrl+E), pressed in order, held a moment, released in reverse;
//! - `XP_TAP=<vk>`: tap that key every two seconds (a way to skip intro movies).

use std::env;
use std::time::{Duration, Instant};

use crate::cpu::Emu;

pub struct Script {
    /// (second to act at, x, y, right button), the next one last.
    clicks: Vec<(u64, i32, i32, bool)>,
    /// When to let go of the button pressed by a click, where, and its button-up message.
    release_at: Option<(Instant, i32, i32, u32)>,
    /// (second to act at, virtual keys), the next one last.
    keys: Vec<(u64, Vec<u32>)>,
    keys_up: Option<(Instant, Vec<u32>)>,
    tap: Option<u32>,
    tapped: u128,
}

impl Script {
    pub fn from_env() -> Script {
        let mut clicks: Vec<(u64, i32, i32, bool)> = env::var("XP_CLICKS")
            .map(|v| {
                v.split(',')
                    .filter_map(|c| {
                        let f: Vec<&str> = c.split(':').map(str::trim).collect();
                        Some((
                            f.first()?.parse().ok()?,
                            f.get(1)?.parse().ok()?,
                            f.get(2)?.parse().ok()?,
                            f.get(3) == Some(&"r"),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        clicks.reverse();
        let mut keys: Vec<(u64, Vec<u32>)> = env::var("XP_KEYS")
            .map(|v| {
                v.split(',')
                    .filter_map(|k| {
                        let (t, vks) = k.split_once(':')?;
                        let vks: Vec<u32> = vks
                            .split('+')
                            .filter_map(|vk| vk.trim().parse().ok())
                            .collect();
                        Some((t.trim().parse().ok()?, vks))
                    })
                    .collect()
            })
            .unwrap_or_default();
        keys.reverse();
        Script {
            clicks,
            release_at: None,
            keys,
            keys_up: None,
            tap: env::var("XP_TAP").ok().and_then(|v| v.parse().ok()),
            tapped: 0,
        }
    }

    /// Deliver whatever is due `started.elapsed()` into the emulated desktop.
    pub fn poll(&mut self, emu: &mut Emu, started: Instant) {
        if let Some(vk) = self.tap
            && started.elapsed().as_millis() / 2000 != self.tapped
        {
            self.tapped = started.elapsed().as_millis() / 2000;
            emu.wm.post_key(vk, true);
            emu.wm.post_key(vk, false);
        }
        while self
            .keys
            .last()
            .is_some_and(|k| started.elapsed().as_secs() >= k.0)
        {
            let (_, vks) = self.keys.pop().unwrap();
            for &vk in &vks {
                emu.wm.post_key(vk, true);
            }
            self.keys_up = Some((Instant::now() + Duration::from_millis(100), vks));
        }
        if let Some((_, vks)) = self.keys_up.take_if(|(at, _)| Instant::now() >= *at) {
            for &vk in vks.iter().rev() {
                emu.wm.post_key(vk, false);
            }
        }
        while self
            .clicks
            .last()
            .is_some_and(|c| started.elapsed().as_secs() >= c.0)
        {
            let (_, x, y, right) = self.clicks.pop().unwrap();
            // WM_LBUTTONDOWN / WM_RBUTTONDOWN; the button-up message is the next one.
            let down = if right { 0x0204 } else { 0x0201 };
            emu.wm.post_mouse(0x0200, x, y); // WM_MOUSEMOVE
            emu.wm.post_mouse(down, x, y);
            // The button is held a moment, as a hand would: games that poll once a frame
            // must see it down.
            self.release_at = Some((Instant::now() + Duration::from_millis(100), x, y, down + 1));
        }
        if let Some((at, x, y, up)) = self.release_at
            && Instant::now() >= at
        {
            emu.wm.post_mouse(up, x, y);
            self.release_at = None;
        }
    }
}
