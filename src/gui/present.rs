//! Shows the emulated desktop in a real macOS window and feeds mouse/keyboard input back.
//! All window-system code lives here so everything else stays testable headless.

use std::sync::{Arc, Mutex};

use minifb::{
    InputCallback, Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions,
};

use crate::cpu::Emu;
use crate::gui::wm::{WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};

struct Chars(Arc<Mutex<Vec<u32>>>);

impl InputCallback for Chars {
    fn add_char(&mut self, c: u32) {
        if let Ok(mut v) = self.0.lock() {
            v.push(c);
        }
    }
}

/// Direct calls into AppKit (Apple Silicon only; see the module). Elsewhere, the stand-ins
/// below.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod cocoa;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod cocoa {
    use minifb::Window;

    pub fn lock_aspect(_: &Window, _: u32, _: u32) -> bool {
        false
    }
    pub struct Rect;
    pub fn layout(_: &Window, _: (u32, u32)) -> Option<Rect> {
        Some(Rect)
    }
    pub fn mouse_in_view(_: &Window) -> Option<((f64, f64), (f64, f64))> {
        None
    }
    pub struct Saved;
    pub fn enter_fullscreen(_: &Window) -> Option<Saved> {
        None
    }
    pub fn exit_fullscreen(_: &Window, _: &Saved) {}
    pub fn show_menu_bar(_: bool) {}
    pub fn hide_menu_bar_in_fullscreen(_: &Window) {}
    pub fn in_native_fullscreen(_: &Window) -> bool {
        false
    }
    pub fn toggle_native_fullscreen(_: &Window) {}
    pub fn keep_awake() {}
    pub fn describe_hit(_: &Window) -> String {
        String::new()
    }
    pub fn modifiers() -> Option<([bool; 6], bool)> {
        None
    }
    pub fn is_key_window(_: &Window) -> bool {
        true
    }
    pub fn on_active_space(_: &Window) -> bool {
        true
    }
    pub fn install_menu() {}
    pub fn presentation_state(_: &Window) -> Option<String> {
        None
    }
    pub fn debug_resize(_: &Window, _: f64, _: f64) {}
    pub fn describe(_: &Window) -> String {
        String::new()
    }
}

/// A resizable window whose picture fills it completely (no letterbox bars): the OS-level
/// aspect lock keeps the proportions, so nothing visibly distorts.
fn open(
    title: &str,
    size: (u32, u32),
    chars: &Arc<Mutex<Vec<u32>>>,
) -> Result<Window, minifb::Error> {
    let options = WindowOptions {
        resize: true,
        scale_mode: ScaleMode::Stretch,
        ..WindowOptions::default()
    };
    let mut window = Window::new(title, size.0 as usize, size.1 as usize, options)?;
    // No frame cap inside minifb: it would sleep in every update, and the emulator would sit
    // idle half the time (the game's own sound mixer then falls behind). `Presenter::due`
    // paces the updates instead.
    window.set_target_fps(0);
    window.set_input_callback(Box::new(Chars(chars.clone())));
    cocoa::hide_menu_bar_in_fullscreen(&window);
    Ok(window)
}

/// Map a window point to game coordinates when the picture fills the whole window.
pub fn window_to_game_stretch(
    mx: f32,
    my: f32,
    win: (f32, f32),
    game: (u32, u32),
) -> Option<(i32, i32)> {
    if win.0 <= 0.0 || win.1 <= 0.0 || game.0 == 0 || game.1 == 0 {
        return None;
    }
    let (x, y) = (mx * game.0 as f32 / win.0, my * game.1 as f32 / win.1);
    (x >= 0.0 && y >= 0.0 && x < game.0 as f32 && y < game.1 as f32).then_some((x as i32, y as i32))
}

/// What passes between the emulator, on its own thread, and the window, on the main thread.
/// Neither waits for the other: the window can block (macOS holds up drawing while a Space
/// switch hides it) and the game, its sound mixer included, carries on.
#[derive(Default)]
pub struct Shared {
    /// The newest picture, replaced by the emulator ~60 times a second.
    frame: Mutex<Option<Frame>>,
    /// Input from the window, waiting for the emulator.
    input: Mutex<Vec<Input>>,
}

struct Frame {
    /// The emulated windows' content, cropped to `region`.
    pixels: Vec<u32>,
    /// Where that content is on the emulated desktop (None: nothing is showing).
    region: Option<(i32, i32, u32, u32)>,
    title: Option<String>,
}

enum Input {
    Mouse(u32, i32, i32),
    /// (virtual key, sided key, extended, down)
    Key(u32, u32, bool, bool),
    Char(u32),
    /// The wheel turned by this much (120 per notch) at a desktop position.
    Wheel(i32, i32, i32),
    /// Left Shift went down or up: speed up while it is held.
    Turbo(bool),
    /// The host window was closed.
    Close,
}

impl Shared {
    /// Emulator side: hand the window the current picture.
    pub fn publish(&self, emu: &mut Emu) {
        // Double clicks are timed against the host's real clock.
        emu.wm.now_ms = emu.wm.now_ms.max(emu.win.elapsed_ms());
        crate::d3d::d3d8::finish_present(emu, false);
        let region = emu.wm.content_bounds();
        let title = emu
            .wm
            .windows
            .iter()
            .find(|w| w.parent == 0 && w.visible)
            .map(|w| w.title.clone());
        let fb = emu.wm.compose();
        let (rx, ry, rw, rh) = region.unwrap_or((0, 0, 0, 0));
        let mut pixels = Vec::with_capacity((rw * rh) as usize);
        for y in 0..rh {
            let row = ((ry as u32 + y) * fb.width + rx as u32) as usize;
            pixels.extend_from_slice(&fb.pixels[row..row + rw as usize]);
        }
        if emu.win.turbo && rw > 0 {
            let mut shot = crate::gui::gfx::Framebuffer {
                width: rw,
                height: rh,
                pixels,
            };
            emu.wm
                .draw_overlay(&mut shot, &format!("{:.1}x", emu.win.speedup));
            pixels = shot.pixels;
        }
        *self.frame.lock().unwrap() = Some(Frame {
            pixels,
            region,
            title,
        });
    }

    /// Emulator side: deliver the window's input. True once the window has been closed.
    pub fn deliver(&self, emu: &mut Emu) -> bool {
        let input = std::mem::take(&mut *self.input.lock().unwrap());
        let mut closed = false;
        for i in input {
            match i {
                Input::Mouse(m, x, y) => emu.wm.post_mouse(m, x, y),
                Input::Key(vk, side, ext, down) => emu.wm.post_key_ex(vk, side, ext, down),
                Input::Wheel(d, x, y) => emu.wm.post_wheel(d, x, y),
                Input::Char(c) => emu.wm.post_char(c),
                Input::Turbo(on) => emu.win.turbo = on,
                Input::Close => closed = true,
            }
        }
        closed
    }
}

/// The Emulator menu's Full Screen was chosen.
static FULLSCREEN_REQUEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Fullscreen, the mouse within this many points of the top edge is resting at the menu bar's
/// trigger (the map scrolls up there too)...
const FS_TOP_MENU: f64 = 2.0;
/// ...and within this many points it scrolls the map up.
const FS_TOP_PAD: f64 = 64.0;
/// How long the mouse rests at the very top before the menu bar is shown.
const FS_MENU_DWELL: std::time::Duration = std::time::Duration::from_secs(1);

pub struct Presenter {
    window: Window,
    chars: Arc<Mutex<Vec<u32>>>,
    left_down: bool,
    /// Input gathered during a pump, handed to the emulator at its end.
    out: Vec<Input>,
    /// The latest picture from the emulator, at `region`'s size.
    shown: Vec<u32>,
    /// The right and middle buttons, as last sent to the game.
    other_down: [bool; 2],
    /// The left button is down as a Control-click (sent as the right button).
    ctrl_click: bool,
    /// Ctrl is hidden from the game after a Control-click, until Control is released.
    ctrl_click_hold: bool,
    logged_buttons: [bool; 3],
    /// The Windows modifier keys (VK_LSHIFT..VK_RMENU) as last sent to the game.
    modifiers: [bool; 6],
    last_pos: (i32, i32),
    /// Trackpad and wheel scrolling not yet worth a whole notch.
    scroll: f32,
    /// Fullscreen: since when the mouse has rested at the very top, and whether the menu
    /// bar is shown over the game.
    top_since: Option<std::time::Instant>,
    menu_shown: bool,
    /// Top-left of the visible region in emulated-desktop coordinates, and its size.
    region: (i32, i32, u32, u32),
    title: String,
    frames: u64,
    last_pump: std::time::Instant,
    /// Whether the content area / drawing view have been sized for the current window.
    fitted: bool,
    /// Set while fullscreen: the window's windowed style and frame, to restore on exit.
    fs: Option<cocoa::Saved>,
    start_fullscreen: bool,
}

impl Presenter {
    pub fn new(title: &str, fullscreen: bool) -> Result<Presenter, minifb::Error> {
        cocoa::keep_awake();
        let chars = Arc::new(Mutex::new(Vec::new()));
        let region = (0, 0, 640, 480);
        let window = open(title, (region.2, region.3), &chars)?;
        cocoa::install_menu();
        Ok(Presenter {
            window,
            chars,
            left_down: false,
            out: Vec::new(),
            shown: Vec::new(),
            other_down: [false; 2],
            ctrl_click: false,
            ctrl_click_hold: false,
            logged_buttons: [false; 3],
            modifiers: [false; 6],
            last_pos: (-1, -1),
            scroll: 0.0,
            top_since: None,
            menu_shown: false,
            region,
            title: title.to_string(),
            frames: 0,
            last_pump: std::time::Instant::now(),
            fitted: false,
            fs: None,
            start_fullscreen: fullscreen,
        })
    }

    /// Time since the last pump.
    pub fn since_pump(&self) -> std::time::Duration {
        self.last_pump.elapsed()
    }

    /// Tell the game about modifier keys that went down or up since last time.
    fn send_modifiers(&mut self, now: [bool; 6]) {
        for (i, (&was, &is)) in self.modifiers.iter().zip(&now).enumerate() {
            if was != is {
                // (generic key, sided key, extended): VK_SHIFT/VK_LSHIFT, ... right Ctrl and
                // right Alt are extended keys.
                let (vk, side, ext) = [
                    (0x10, 0xA0, false),
                    (0x10, 0xA1, false),
                    (0x11, 0xA2, false),
                    (0x11, 0xA3, true),
                    (0x12, 0xA4, false),
                    (0x12, 0xA5, true),
                ][i];
                self.out.push(Input::Key(vk, side, ext, is));
                if i == 0 {
                    self.out.push(Input::Turbo(is));
                }
            }
        }
        self.modifiers = now;
    }

    /// Show the emulator's newest picture and pass on pending input. The picture is exactly the
    /// emulated windows' content (no emulated frame), filling the host window. Returns
    /// false once the host window has been closed.
    pub fn pump(&mut self, shared: &Shared) -> bool {
        self.last_pump = std::time::Instant::now();
        self.frames += 1;
        let debug = std::env::var_os("XP_DEBUG_TOGGLE").is_some();
        // Our instant fullscreen, or macOS's own (green button).
        let was_fullscreen = self.fs.is_some() || cocoa::in_native_fullscreen(&self.window);

        let mut want = self.region;
        if let Some(f) = shared.frame.lock().unwrap().take() {
            want = f.region.unwrap_or((0, 0, self.region.2, self.region.3));
            if f.region.is_some() {
                self.shown = f.pixels;
            }
            if let Some(t) = f.title {
                self.title = t;
            }
        }
        let resized = (want.2, want.3) != (self.region.2, self.region.3);
        self.region = want;
        // Follow the game window's size by making a fresh host window; never while in
        // fullscreen, where the system owns the size.
        if resized
            && !was_fullscreen
            && let Ok(w) = open(&self.title, (want.2, want.3), &self.chars)
        {
            self.window = w;
            self.fitted = false;
        }
        let (rx, ry, rw, rh) = self.region;

        // Lay the drawing view out over what AppKit really shows (see `cocoa::layout`).
        if cocoa::layout(&self.window, (rw, rh), was_fullscreen).is_some() && !self.fitted {
            self.fitted = true;
            if !cocoa::lock_aspect(&self.window, rw, rh) {
                eprintln!("[present] could not lock the window's aspect ratio");
            }
        }

        if self.shown.len() == (rw * rh) as usize && rw > 0 {
            let _ = self
                .window
                .update_with_buffer(&self.shown, rw as usize, rh as usize);
        } else {
            self.window.update();
        }

        // Prefer macOS's own mouse position relative to the drawing view; fall back to
        // minifb's where that is unavailable.
        let mouse = match cocoa::mouse_in_view(&self.window) {
            Some(((mx, my), (vw, vh))) => {
                // Fullscreen, the whole screen belongs to the game: the mouse over the black
                // bars beside the picture is at the picture's edge, so edge scrolling works on
                // every side.
                let (mx, my) = if was_fullscreen {
                    let my = my.clamp(0.0, vh - 1.0);
                    // Resting at the very top for a second shows the menu bar over the game;
                    // moving below the scroll band hides it again.
                    if self.fs.is_some() {
                        if my >= FS_TOP_PAD {
                            self.top_since = None;
                            if std::mem::take(&mut self.menu_shown) {
                                cocoa::show_menu_bar(false);
                            }
                        } else if my < FS_TOP_MENU {
                            let since = *self.top_since.get_or_insert_with(std::time::Instant::now);
                            if !self.menu_shown && since.elapsed() >= FS_MENU_DWELL {
                                self.menu_shown = true;
                                cocoa::show_menu_bar(true);
                            }
                        } else if !self.menu_shown {
                            self.top_since = None;
                        }
                    }
                    // The band along the top, down to the menu bar's trigger, scrolls the map
                    // up, the very top edge included.
                    let my = if my < FS_TOP_PAD { 0.0 } else { my };
                    (mx.clamp(0.0, vw - 1.0), my)
                } else {
                    (mx, my)
                };
                window_to_game_stretch(mx as f32, my as f32, (vw as f32, vh as f32), (rw, rh))
            }
            None => {
                let (ww, wh) = self.window.get_size();
                self.window
                    .get_unscaled_mouse_pos(MouseMode::Discard)
                    .and_then(|(mx, my)| {
                        window_to_game_stretch(mx, my, (ww as f32, wh as f32), (rw, rh))
                    })
            }
        };
        // Modifiers, Mac to PC: Control is Ctrl, Option is Alt; Command is the Mac's own and
        // the game never sees it. They are read from the keyboard's state as macOS reports it
        // right now, not from key events: a release can go missing (a fullscreen switch,
        // Command-Tab), and a modifier left held turns every key into a shortcut. Nothing is
        // held while the window is not in front.
        let (mut now, command) = if self.window.is_active() || cocoa::is_key_window(&self.window) {
            cocoa::modifiers().unwrap_or_else(|| {
                let held = |keys: &[Key]| keys.iter().any(|&k| self.window.is_key_down(k));
                (
                    [
                        held(&[Key::LeftShift]),
                        held(&[Key::RightShift]),
                        held(&[Key::LeftCtrl]),
                        held(&[Key::RightCtrl]),
                        held(&[Key::LeftAlt]),
                        held(&[Key::RightAlt]),
                    ],
                    held(&[Key::LeftSuper, Key::RightSuper]),
                )
            })
        } else {
            ([false; 6], false)
        };
        let control = now[2] || now[3];
        // Control-click is a right click, as everywhere on the Mac: the game sees the right
        // button and not Ctrl, until Control is let go.
        if !control {
            self.ctrl_click_hold = false;
        }
        if self.ctrl_click_hold {
            now[2] = false;
            now[3] = false;
        }

        // XP_INPUT_LOG=1: the mouse buttons as minifb sees them, and where the mouse is.
        if std::env::var_os("XP_INPUT_LOG").is_some() {
            if self.frames % 240 == 0 {
                eprintln!("[input] {}", cocoa::describe_hit(&self.window));
            }
            let b = [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
                .map(|m| self.window.get_mouse_down(m));
            if b != self.logged_buttons {
                self.logged_buttons = b;
                eprintln!(
                    "[input] buttons {b:?} game pos {mouse:?} view {:?} fullscreen {was_fullscreen}",
                    cocoa::mouse_in_view(&self.window)
                        .map(|((x, y), (w, h))| (x as i32, y as i32, w as i32, h as i32))
                );
            }
        }
        // The game's window is on another Space: the mouse is not over it.
        let mouse = mouse.filter(|_| cocoa::on_active_space(&self.window));
        if let Some((mx, my)) = mouse {
            let pos = (mx + rx, my + ry);
            if pos != self.last_pos {
                self.last_pos = pos;
                self.out.push(Input::Mouse(WM_MOUSEMOVE, pos.0, pos.1));
            }
            // Trackpad scrolling is a wheel turning in and out: a notch per half line.
            if let Some((_, dy)) = self.window.get_scroll_wheel() {
                self.scroll += dy;
                let notches = (self.scroll / 0.5) as i32;
                if notches != 0 {
                    self.scroll -= notches as f32 * 0.5;
                    self.out.push(Input::Wheel(notches * 120, pos.0, pos.1));
                }
            }
            let down = self.window.get_mouse_down(MouseButton::Left);
            if down != self.left_down {
                self.left_down = down;
                if down && control {
                    // Control-click: let go of Ctrl first, then press the right button.
                    self.ctrl_click_hold = true;
                    self.ctrl_click = true;
                    now[2] = false;
                    now[3] = false;
                    self.send_modifiers(now);
                    self.out.push(Input::Mouse(0x0204, pos.0, pos.1)); // WM_RBUTTONDOWN
                } else if !down && self.ctrl_click {
                    self.ctrl_click = false;
                    self.out.push(Input::Mouse(0x0205, pos.0, pos.1)); // WM_RBUTTONUP
                } else {
                    self.out.push(Input::Mouse(
                        if down { WM_LBUTTONDOWN } else { WM_LBUTTONUP },
                        pos.0,
                        pos.1,
                    ));
                }
            }
            // Right and middle buttons (WM_RBUTTONDOWN/UP, WM_MBUTTONDOWN/UP).
            for (i, button, msg) in [
                (0, MouseButton::Right, 0x0204),
                (1, MouseButton::Middle, 0x0207),
            ] {
                let down = self.window.get_mouse_down(button);
                if down != self.other_down[i] {
                    self.other_down[i] = down;
                    self.out
                        .push(Input::Mouse(if down { msg } else { msg + 1 }, pos.0, pos.1));
                }
            }
        } else if self.left_down && !self.window.get_mouse_down(MouseButton::Left) {
            self.left_down = false; // released outside the picture: don't leave a drag stuck
            let up = if std::mem::take(&mut self.ctrl_click) {
                0x0205
            } else {
                WM_LBUTTONUP
            };
            self.out
                .push(Input::Mouse(up, self.last_pos.0, self.last_pos.1));
        }

        // Option+Enter toggles macOS fullscreen and is not forwarded to the game.
        // (XP_DEBUG_TOGGLE=1 also flips it at frames 120 and 300, to inspect the transition.)
        let auto = debug && (self.frames == 120 || self.frames == 300);
        let start = self.start_fullscreen && self.fitted && self.frames > 5;
        let toggle = auto
            || start
            || FULLSCREEN_REQUEST.swap(false, std::sync::atomic::Ordering::Relaxed)
            || ((self.window.is_key_down(Key::LeftAlt) || self.window.is_key_down(Key::RightAlt))
                && self.window.is_key_pressed(Key::Enter, KeyRepeat::No));
        if toggle {
            self.start_fullscreen = false;
            match self.fs.take() {
                Some(saved) => {
                    self.menu_shown = false;
                    self.top_since = None;
                    cocoa::exit_fullscreen(&self.window, &saved)
                }
                None => self.fs = cocoa::enter_fullscreen(&self.window),
            }
        }
        // XP_FS_DEBUG=1 reports the presentation options; =native also presses the green
        // button early on.
        if std::env::var_os("XP_FS_DEBUG").is_some_and(|v| v == "native") && self.frames == 60 {
            cocoa::toggle_native_fullscreen(&self.window);
        }
        if std::env::var_os("XP_FS_DEBUG").is_some() && self.frames % 120 == 0 {
            eprintln!(
                "[present] frame {} fullscreen={} {:?} modifiers {:?} sent {:?}",
                self.frames,
                was_fullscreen,
                cocoa::presentation_state(&self.window),
                cocoa::modifiers(),
                self.modifiers
            );
        }
        if debug && self.frames == 360 {
            cocoa::debug_resize(&self.window, 900.0, 618.0);
        }
        if debug && [60, 130, 200, 310, 400, 420].contains(&self.frames) {
            eprintln!(
                "[present] frame {} fullscreen={} region={:?} mouse(view-relative, view size)={:?} :: {}",
                self.frames,
                was_fullscreen,
                self.region,
                cocoa::mouse_in_view(&self.window),
                cocoa::describe(&self.window)
            );
        }
        self.send_modifiers(now);
        for key in self.window.get_keys_pressed(KeyRepeat::Yes) {
            // Command shortcuts are the Mac's (Command-Tab, Command-Q): not keys for the game.
            if (toggle && key == Key::Enter) || command {
                continue;
            }
            if let Some((vk, ext)) = virtual_key(key) {
                self.out.push(Input::Key(vk, 0, ext, true));
            }
        }
        for key in self.window.get_keys_released() {
            if let Some((vk, ext)) = virtual_key(key) {
                self.out.push(Input::Key(vk, 0, ext, false));
            }
        }
        let typed: Vec<u32> = self
            .chars
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        let ctrl = self.modifiers[2] || self.modifiers[3];
        for c in typed {
            if command {
                continue;
            }
            // minifb reports no key for the backtick (` and ~) on macOS, only the character:
            // the key press (VK_OEM_3) is made up from it, as the game's console opens on it.
            if c == '`' as u32 || c == '~' as u32 {
                self.out.push(Input::Key(0xC0, 0, false, true));
                self.out.push(Input::Key(0xC0, 0, false, false));
            }
            // With Ctrl held, Windows types control characters (Ctrl+A is 1) and nothing for
            // other keys.
            if ctrl {
                if let Some(l) = char::from_u32(c).filter(char::is_ascii_alphabetic) {
                    self.out
                        .push(Input::Char(l.to_ascii_uppercase() as u32 & 0x1F));
                }
                continue;
            }
            self.out.push(Input::Char(c));
        }
        let open = self.window.is_open();
        if !open {
            self.out.push(Input::Close);
        }
        shared.input.lock().unwrap().append(&mut self.out);
        open
    }
}

/// Windows virtual-key code for a host key (modifiers are handled separately), and whether
/// it is an extended key on a PC keyboard.
pub fn virtual_key(key: Key) -> Option<(u32, bool)> {
    use Key::*;
    let vk = match key {
        Key0 | Key1 | Key2 | Key3 | Key4 | Key5 | Key6 | Key7 | Key8 | Key9 => {
            0x30 + (key as u32 - Key0 as u32)
        }
        A | B | C | D | E | F | G | H | I | J | K | L | M | N | O | P | Q | R | S | T | U | V
        | W | X | Y | Z => 0x41 + (key as u32 - A as u32),
        F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12 => {
            0x70 + (key as u32 - F1 as u32)
        }
        Left => 0x25,
        Up => 0x26,
        Right => 0x27,
        Down => 0x28,
        Backspace => 0x08,
        Tab => 0x09,
        Enter => 0x0D,
        Escape => 0x1B,
        Space => 0x20,
        PageUp => 0x21,
        PageDown => 0x22,
        End => 0x23,
        Home => 0x24,
        Insert => 0x2D,
        Delete => 0x2E,
        Minus => 0xBD,
        Equal => 0xBB,
        LeftBracket => 0xDB,
        RightBracket => 0xDD,
        Backslash => 0xDC,
        Semicolon => 0xBA,
        Apostrophe => 0xDE,
        Comma => 0xBC,
        Period => 0xBE,
        Slash => 0xBF,
        Backquote => 0xC0,
        CapsLock => 0x14,
        Pause => 0x13,
        ScrollLock => 0x91,
        NumLock => 0x90,
        Menu => 0x5D,
        NumPad0 | NumPad1 | NumPad2 | NumPad3 | NumPad4 | NumPad5 | NumPad6 | NumPad7 | NumPad8
        | NumPad9 => 0x60 + (key as u32 - NumPad0 as u32),
        NumPadDot => 0x6E,
        NumPadSlash => 0x6F,
        NumPadAsterisk => 0x6A,
        NumPadMinus => 0x6D,
        NumPadPlus => 0x6B,
        NumPadEnter => return Some((0x0D, true)),
        _ => return None,
    };
    Some((vk, crate::gui::keyboard::scan_code(vk).is_some_and(|s| s.1)))
}

#[cfg(test)]
mod tests {
    use super::window_to_game_stretch;

    #[test]
    fn stretch_mapping_fills_the_window() {
        assert_eq!(
            window_to_game_stretch(0.0, 0.0, (1182.0, 818.0), (591, 409)),
            Some((0, 0))
        );
        assert_eq!(
            window_to_game_stretch(1181.0, 817.0, (1182.0, 818.0), (591, 409)),
            Some((590, 408))
        );
        assert_eq!(
            window_to_game_stretch(591.0, 409.0, (1182.0, 818.0), (591, 409)),
            Some((295, 204))
        );
        assert_eq!(
            window_to_game_stretch(1182.0, 10.0, (1182.0, 818.0), (591, 409)),
            None
        );
        assert_eq!(
            window_to_game_stretch(-1.0, 10.0, (1182.0, 818.0), (591, 409)),
            None
        );
        assert_eq!(
            window_to_game_stretch(1.0, 1.0, (0.0, 0.0), (591, 409)),
            None
        );
    }

    #[test]
    fn unscaled_window_maps_one_to_one() {
        assert_eq!(
            window_to_game_stretch(10.0, 20.0, (640.0, 480.0), (640, 480)),
            Some((10, 20))
        );
        assert_eq!(
            window_to_game_stretch(640.0, 0.0, (640.0, 480.0), (640, 480)),
            None
        );
    }
}
