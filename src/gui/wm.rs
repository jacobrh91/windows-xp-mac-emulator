//! Window manager: window tree, message queue, timers, GDI objects and a compositor
//! that draws everything onto one desktop framebuffer. Pure state — no guest memory
//! and no host windowing — so it is testable headless. `gui::user32` is the Win32 face.

use std::collections::{HashMap, VecDeque};

use crate::gui::gfx::{Font, Framebuffer};

pub const DESKTOP_W: u32 = 1024;
pub const DESKTOP_H: u32 = 768;
const DESKTOP_COLOR: u32 = 0x3A6EA5;
const BTN_FACE: u32 = 0xD4D0C8;
const CAPTION_COLOR: u32 = 0x0054E3;
const CAPTION_H: i32 = 19;
const FRAME_W: i32 = 3;

pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_MOVE: u32 = 0x0003;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_SETTEXT: u32 = 0x000C;
pub const WM_GETTEXT: u32 = 0x000D;
pub const WM_GETTEXTLENGTH: u32 = 0x000E;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_SHOWWINDOW: u32 = 0x0018;
pub const WM_NCCREATE: u32 = 0x0081;
pub const WM_NCHITTEST: u32 = 0x0084;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_CHAR: u32 = 0x0102;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_LBUTTONDBLCLK: u32 = 0x0203;
/// Window class style: deliver double clicks as WM_LBUTTONDBLCLK.
pub const CS_DBLCLKS: u32 = 0x0008;
/// Two presses count as a double click within this time and distance (Windows defaults).
const DBLCLK_MS: u64 = 500;
const DBLCLK_PX: i32 = 4;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_SYSKEYDOWN: u32 = 0x0104;
pub const WM_SYSKEYUP: u32 = 0x0105;
pub const WM_INITMENUPOPUP: u32 = 0x0117;
pub const WM_INITDIALOG: u32 = 0x0110;

pub const MENU_H: i32 = 18;
pub const MF_GRAYED: u32 = 0x0001;
pub const MF_CHECKED: u32 = 0x0008;
pub const MF_POPUP: u32 = 0x0010;
pub const MF_END: u32 = 0x0080;
pub const MF_SEPARATOR: u32 = 0x0800;

pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_CAPTION: u32 = 0x00C0_0000;
pub const WS_BORDER: u32 = 0x0080_0000;
pub const CW_USEDEFAULT: u32 = 0x8000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Msg {
    pub hwnd: u32,
    pub message: u32,
    pub wparam: u32,
    pub lparam: u32,
    pub time: u32,
}

#[derive(Clone)]
pub struct WndClass {
    pub wndproc: u32,
    pub bg: u32,
    /// lpszMenuName as given at registration (id or string pointer), 0 for none.
    pub menu_name: u32,
    pub wide: bool,
    /// CS_* class styles.
    pub style: u32,
}

pub struct Window {
    pub hwnd: u32,
    pub class: String,
    pub parent: u32,
    pub style: u32,
    pub ex_style: u32,
    /// Outer rectangle; relative to the desktop for top-level windows, to the parent's
    /// client area for children.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub title: String,
    pub visible: bool,
    pub enabled: bool,
    pub wndproc: u32,
    pub userdata: u32,
    /// Control ID (hMenu) for child windows.
    pub id: u32,
    pub needs_paint: bool,
    /// Whether WM_SIZE/WM_MOVE have been delivered yet (they go out on first show).
    pub geometry_sent: bool,
    /// Menu bar handle (top-level windows only), or 0.
    pub menu: u32,
    /// Dialog windows route messages through a DLGPROC (nonzero = handled) and then the
    /// default dialog handling.
    pub is_dialog: bool,
    /// Button state: 0 unchecked, 1 checked, 2 indeterminate.
    pub check: u32,
    pub bg: u32,
    pub surface: Framebuffer,
}

pub struct MenuItem {
    pub text: String,
    pub id: u32,
    /// MF_GRAYED | MF_CHECKED | MF_SEPARATOR
    pub flags: u32,
    /// Handle of the dropdown menu, or 0.
    pub submenu: u32,
}

#[derive(Default)]
pub struct Menu {
    pub items: Vec<MenuItem>,
}

pub struct Accel {
    pub flags: u8,
    pub key: u16,
    pub id: u16,
}

/// A DialogBox in progress: it stays pending until EndDialog supplies a result.
pub struct Modal {
    pub hwnd: u32,
    pub result: Option<i32>,
}

pub struct OpenMenu {
    pub hwnd: u32,
    pub index: usize,
}

pub struct Timer {
    pub hwnd: u32,
    pub id: u32,
    pub interval: u64,
    pub due: u64,
    pub callback: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Window(u32),
    Mem,
}

#[derive(Clone)]
pub struct Dc {
    pub target: Target,
    pub text_color: u32,
    pub bk_color: u32,
    pub transparent: bool,
    pub pen: u32,
    pub brush: u32,
    pub font: u32,
    pub bitmap: u32,
    pub pos: (i32, i32),
}

pub enum Gdi {
    Pen { color: u32, width: i32, null: bool },
    Brush { color: u32, null: bool },
    Font { px: u32 },
    Bitmap(Framebuffer),
    Dc(Dc),
}

pub mod stock {
    pub const WHITE_BRUSH: u32 = 0x8000_0001;
    pub const LTGRAY_BRUSH: u32 = 0x8000_0002;
    pub const GRAY_BRUSH: u32 = 0x8000_0003;
    pub const DKGRAY_BRUSH: u32 = 0x8000_0004;
    pub const BLACK_BRUSH: u32 = 0x8000_0005;
    pub const NULL_BRUSH: u32 = 0x8000_0006;
    pub const WHITE_PEN: u32 = 0x8000_0007;
    pub const BLACK_PEN: u32 = 0x8000_0008;
    pub const NULL_PEN: u32 = 0x8000_0009;
    pub const FONT: u32 = 0x8000_000A;
}

/// Convert a Win32 COLORREF (0x00BBGGRR) to our 0x00RRGGBB.
pub fn colorref(c: u32) -> u32 {
    ((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff)
}

pub fn sys_color(index: u32) -> u32 {
    match index {
        1 => DESKTOP_COLOR,
        2 => CAPTION_COLOR,
        5 | 8 => 0xFFFFFF,       // WINDOW, WINDOWTEXT is below
        6 => 0x000000,           // WINDOWFRAME
        7 | 18 => 0x000000,      // MENUTEXT, BTNTEXT
        15 | 4 | 22 => BTN_FACE, // BTNFACE, MENU, BTNSHADOW area
        16 => 0x808080,          // BTNSHADOW
        20 => 0xFFFFFF,          // BTNHILIGHT
        _ => BTN_FACE,
    }
}

pub struct WindowManager {
    pub classes: HashMap<String, WndClass>,
    /// Registered class names in order; the atom is `0xC000 + index`.
    pub class_atoms: Vec<String>,
    pub windows: Vec<Window>,
    next_hwnd: u32,
    pub queue: VecDeque<Msg>,
    pub quit: Option<u32>,
    pub timers: Vec<Timer>,
    pub now_ms: u64,
    pub gdi: HashMap<u32, Gdi>,
    next_gdi: u32,
    pub desktop: Framebuffer,
    font: Option<Font>,
    pub focus: u32,
    /// The active (foreground) top-level window.
    pub active: u32,
    pub capture: u32,
    pub menus: HashMap<u32, Menu>,
    next_menu: u32,
    pub open_menu: Option<OpenMenu>,
    pub accels: HashMap<u32, Vec<Accel>>,
    pub key_down: [bool; 256],
    /// Mouse buttons held, as MK_LBUTTON / MK_RBUTTON / MK_MBUTTON bits.
    pub buttons: u32,
    pub modal: Option<Modal>,
    /// The last left-button press: (time ms, x, y, window), for double-click detection.
    last_click: Option<(u64, i32, i32, u32)>,
}

impl WindowManager {
    pub fn new() -> Self {
        use stock::*;
        let mut gdi = HashMap::new();
        for (h, c) in [
            (WHITE_BRUSH, 0xFFFFFF),
            (LTGRAY_BRUSH, 0xC0C0C0),
            (GRAY_BRUSH, 0x808080),
            (DKGRAY_BRUSH, 0x404040),
            (BLACK_BRUSH, 0),
        ] {
            gdi.insert(
                h,
                Gdi::Brush {
                    color: c,
                    null: false,
                },
            );
        }
        gdi.insert(
            NULL_BRUSH,
            Gdi::Brush {
                color: 0,
                null: true,
            },
        );
        gdi.insert(
            WHITE_PEN,
            Gdi::Pen {
                color: 0xFFFFFF,
                width: 1,
                null: false,
            },
        );
        gdi.insert(
            BLACK_PEN,
            Gdi::Pen {
                color: 0,
                width: 1,
                null: false,
            },
        );
        gdi.insert(
            NULL_PEN,
            Gdi::Pen {
                color: 0,
                width: 1,
                null: true,
            },
        );
        gdi.insert(FONT, Gdi::Font { px: 13 });
        let mut desktop = Framebuffer::new(DESKTOP_W, DESKTOP_H);
        desktop.fill_rect(0, 0, DESKTOP_W, DESKTOP_H, DESKTOP_COLOR);
        WindowManager {
            classes: HashMap::new(),
            class_atoms: Vec::new(),
            windows: Vec::new(),
            next_hwnd: 0x0001_0010,
            queue: VecDeque::new(),
            quit: None,
            timers: Vec::new(),
            now_ms: 0,
            gdi,
            next_gdi: 0x8100_0000,
            desktop,
            font: None,
            focus: 0,
            active: 0,
            capture: 0,
            menus: HashMap::new(),
            next_menu: 0x9000_0010,
            open_menu: None,
            accels: HashMap::new(),
            key_down: [false; 256],
            buttons: 0,
            modal: None,
            last_click: None,
        }
    }

    // ---- windows ----------------------------------------------------------------------

    pub fn window(&self, hwnd: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.hwnd == hwnd)
    }

    pub fn window_mut(&mut self, hwnd: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.hwnd == hwnd)
    }

    pub fn is_builtin_class(name: &str) -> bool {
        matches!(
            name.to_ascii_uppercase().as_str(),
            "BUTTON" | "STATIC" | "EDIT" | "LISTBOX" | "COMBOBOX" | "SCROLLBAR"
        )
    }

    /// (border, caption height) for a style.
    fn frame(style: u32, child: bool) -> (i32, i32) {
        if child {
            return (if style & WS_BORDER != 0 { 1 } else { 0 }, 0);
        }
        if style & WS_CAPTION == WS_CAPTION {
            (FRAME_W, CAPTION_H)
        } else if style & WS_BORDER != 0 {
            (1, 0)
        } else {
            (0, 0)
        }
    }

    /// (border, caption + menu bar height) of a window.
    fn chrome(w: &Window) -> (i32, i32) {
        let (b, c) = Self::frame(w.style, w.parent != 0);
        (b, c + if w.menu != 0 { MENU_H } else { 0 })
    }

    /// Smallest desktop rectangle (x, y, w, h) covering every visible top-level window,
    /// clamped to the desktop. `None` when nothing is on screen.
    #[cfg(test)]
    pub fn bounds(&self) -> Option<(i32, i32, u32, u32)> {
        let mut it = self.windows.iter().filter(|w| w.parent == 0 && w.visible);
        let first = it.next()?;
        let (mut x0, mut y0, mut x1, mut y1) =
            (first.x, first.y, first.x + first.w, first.y + first.h);
        for w in it {
            x0 = x0.min(w.x);
            y0 = y0.min(w.y);
            x1 = x1.max(w.x + w.w);
            y1 = y1.max(w.y + w.h);
        }
        let (x0, y0) = (x0.clamp(0, DESKTOP_W as i32), y0.clamp(0, DESKTOP_H as i32));
        let (x1, y1) = (x1.clamp(0, DESKTOP_W as i32), y1.clamp(0, DESKTOP_H as i32));
        (x1 > x0 && y1 > y0).then(|| (x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// Like `bounds`, but only the content: each visible top-level window's client area
    /// plus its menu bar, without the emulated border and title bar (the host window
    /// supplies those). Returns (x, y, w, h) in desktop coordinates.
    pub fn content_bounds(&self) -> Option<(i32, i32, u32, u32)> {
        let mut acc: Option<(i32, i32, i32, i32)> = None;
        for w in self.windows.iter().filter(|w| w.parent == 0 && w.visible) {
            let (Some((ox, oy)), Some((cw, ch))) =
                (self.client_origin(w.hwnd), self.client_size(w.hwnd))
            else {
                continue;
            };
            let top = oy - if w.menu != 0 { MENU_H } else { 0 };
            let r = (ox, top, ox + cw, oy + ch);
            acc = Some(match acc {
                None => r,
                Some(a) => (a.0.min(r.0), a.1.min(r.1), a.2.max(r.2), a.3.max(r.3)),
            });
        }
        let (x0, y0, x1, y1) = acc?;
        let (x0, y0) = (x0.clamp(0, DESKTOP_W as i32), y0.clamp(0, DESKTOP_H as i32));
        let (x1, y1) = (x1.clamp(0, DESKTOP_W as i32), y1.clamp(0, DESKTOP_H as i32));
        (x1 > x0 && y1 > y0).then(|| (x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    pub fn client_size(&self, hwnd: u32) -> Option<(i32, i32)> {
        let w = self.window(hwnd)?;
        let (b, c) = Self::chrome(w);
        Some(((w.w - 2 * b).max(0), (w.h - c - 2 * b).max(0)))
    }

    /// Top-left of the client area in desktop coordinates.
    pub fn client_origin(&self, hwnd: u32) -> Option<(i32, i32)> {
        let w = self.window(hwnd)?;
        let (b, c) = Self::chrome(w);
        let (px, py) = if w.parent != 0 {
            self.client_origin(w.parent)?
        } else {
            (0, 0)
        };
        Some((px + w.x + b, py + w.y + b + c))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_window(
        &mut self,
        class: &str,
        title: &str,
        style: u32,
        ex_style: u32,
        pos: (u32, u32, u32, u32),
        parent: u32,
        id: u32,
    ) -> u32 {
        let (mut x, mut y, mut w, mut h) = (pos.0 as i32, pos.1 as i32, pos.2 as i32, pos.3 as i32);
        let n = self.windows.len() as i32;
        if pos.0 == CW_USEDEFAULT {
            x = 40 + 24 * (n % 10);
        }
        if pos.1 == CW_USEDEFAULT {
            y = 40 + 24 * (n % 10);
        }
        if pos.2 == CW_USEDEFAULT {
            w = 640;
        }
        if pos.3 == CW_USEDEFAULT {
            h = 480;
        }
        let key = class.to_ascii_lowercase();
        let (wndproc, class_bg) = match self.classes.get(&key) {
            Some(c) => (c.wndproc, Some(c.bg)),
            None => (0, None),
        };
        let bg = class_bg.unwrap_or(if class.eq_ignore_ascii_case("EDIT") {
            0xFFFFFF
        } else {
            BTN_FACE
        });
        let hwnd = self.next_hwnd;
        self.next_hwnd += 0x10;
        let mut win = Window {
            hwnd,
            class: class.to_string(),
            parent,
            style,
            ex_style,
            x,
            y,
            w,
            h,
            title: title.to_string(),
            visible: false,
            enabled: true,
            wndproc,
            userdata: 0,
            id,
            needs_paint: true,
            geometry_sent: false,
            menu: 0,
            is_dialog: false,
            check: 0,
            bg,
            surface: Framebuffer::new(0, 0),
        };
        let (b, c) = Self::frame(style, parent != 0);
        let (cw, ch) = ((w - 2 * b).max(0) as u32, (h - c - 2 * b).max(0) as u32);
        win.surface = Framebuffer::new(cw, ch);
        win.surface.fill_rect(0, 0, cw, ch, bg);
        self.windows.push(win);
        hwnd
    }

    /// Resize a window's outer rectangle, keeping what it has painted so far.
    pub fn set_geometry(&mut self, hwnd: u32, x: i32, y: i32, w: i32, h: i32) {
        let Some(win) = self.window_mut(hwnd) else {
            return;
        };
        win.x = x;
        win.y = y;
        let resized = (win.w, win.h) != (w, h);
        win.w = w;
        win.h = h;
        if resized {
            let (b, c) = Self::chrome(win);
            let (cw, ch) = ((w - 2 * b).max(0) as u32, (h - c - 2 * b).max(0) as u32);
            let mut fresh = Framebuffer::new(cw, ch);
            fresh.fill_rect(0, 0, cw, ch, win.bg);
            fresh.blit(&win.surface, 0, 0);
            win.surface = fresh;
            win.needs_paint = true;
        }
    }

    pub fn children_of(&self, hwnd: u32) -> Vec<u32> {
        self.windows
            .iter()
            .filter(|w| w.parent == hwnd)
            .map(|w| w.hwnd)
            .collect()
    }

    /// Remove a window and all its descendants; returns every removed handle.
    pub fn remove_window(&mut self, hwnd: u32) -> Vec<u32> {
        let mut gone = vec![hwnd];
        let mut i = 0;
        while i < gone.len() {
            gone.extend(self.children_of(gone[i]));
            i += 1;
        }
        self.windows.retain(|w| !gone.contains(&w.hwnd));
        self.timers.retain(|t| !gone.contains(&t.hwnd));
        self.queue.retain(|m| !gone.contains(&m.hwnd));
        if gone.contains(&self.focus) {
            self.focus = 0;
        }
        gone
    }

    pub fn is_visible(&self, hwnd: u32) -> bool {
        let mut cur = hwnd;
        while cur != 0 {
            match self.window(cur) {
                Some(w) if w.visible => cur = w.parent,
                _ => return false,
            }
        }
        true
    }

    // ---- messages -----------------------------------------------------------------------

    pub fn post(&mut self, hwnd: u32, message: u32, wparam: u32, lparam: u32) {
        let time = self.now_ms as u32;
        self.queue.push_back(Msg {
            hwnd,
            message,
            wparam,
            lparam,
            time,
        });
    }

    /// The next message the app should see, in Windows' priority order: posted messages,
    /// then WM_QUIT, then due timers, then WM_PAINT. Removes it if `remove`.
    pub fn next_message(&mut self, remove: bool) -> Option<Msg> {
        if let Some(m) = if remove {
            self.queue.pop_front()
        } else {
            self.queue.front().copied()
        } {
            return Some(m);
        }
        if let Some(code) = self.quit {
            if remove {
                self.quit = None;
            }
            return Some(Msg {
                hwnd: 0,
                message: WM_QUIT,
                wparam: code,
                lparam: 0,
                time: self.now_ms as u32,
            });
        }
        let now = self.now_ms;
        if let Some(t) = self.timers.iter_mut().find(|t| t.due <= now) {
            if remove {
                t.due = now + t.interval;
            }
            return Some(Msg {
                hwnd: t.hwnd,
                message: WM_TIMER,
                wparam: t.id,
                lparam: t.callback,
                time: now as u32,
            });
        }
        let hwnd = self
            .windows
            .iter()
            .find(|w| w.needs_paint && self.is_visible(w.hwnd))
            .map(|w| w.hwnd)?;
        Some(Msg {
            hwnd,
            message: WM_PAINT,
            wparam: 0,
            lparam: 0,
            time: now as u32,
        })
    }

    pub fn set_timer(&mut self, hwnd: u32, id: u32, interval: u32, callback: u32) -> u32 {
        let id = if hwnd == 0 && id == 0 {
            0x1000 + self.timers.len() as u32
        } else {
            id
        };
        self.timers.retain(|t| !(t.hwnd == hwnd && t.id == id));
        let interval = interval.max(10) as u64;
        self.timers.push(Timer {
            hwnd,
            id,
            interval,
            due: self.now_ms + interval,
            callback,
        });
        id
    }

    pub fn kill_timer(&mut self, hwnd: u32, id: u32) -> bool {
        let before = self.timers.len();
        self.timers.retain(|t| !(t.hwnd == hwnd && t.id == id));
        self.timers.len() != before
    }

    // ---- input from the host ------------------------------------------------------------

    /// Deepest visible window containing the desktop point, as (hwnd, client x, client y).
    pub fn hit_test(&self, x: i32, y: i32) -> Option<(u32, i32, i32)> {
        let mut best: Option<(u32, usize)> = None;
        for (z, w) in self.windows.iter().enumerate() {
            if !self.is_visible(w.hwnd) {
                continue;
            }
            let (Some((ox, oy)), Some((cw, ch))) =
                (self.client_origin(w.hwnd), self.client_size(w.hwnd))
            else {
                continue;
            };
            if x >= ox && y >= oy && x < ox + cw && y < oy + ch {
                // Later windows are above; children come after their parents.
                best = Some((w.hwnd, z));
            }
        }
        let (hwnd, _) = best?;
        let (ox, oy) = self.client_origin(hwnd)?;
        Some((hwnd, x - ox, y - oy))
    }

    pub fn post_mouse(&mut self, message: u32, x: i32, y: i32) {
        let mut message = message;
        if message == WM_LBUTTONDOWN && self.handle_menu_click(x, y) {
            return;
        }
        let target = if self.capture != 0 {
            self.client_origin(self.capture)
                .map(|(ox, oy)| (self.capture, x - ox, y - oy))
        } else {
            self.hit_test(x, y)
        };
        if let Some((hwnd, cx, cy)) = target {
            if message == WM_LBUTTONDOWN {
                self.focus = hwnd;
                message = self.classify_press(hwnd, x, y);
            }
            if message == WM_LBUTTONUP {
                // Built-in buttons turn a click into WM_COMMAND(BN_CLICKED) for their parent.
                if let Some(w) = self.window(hwnd).filter(|w| {
                    w.wndproc == 0 && w.enabled && w.class.eq_ignore_ascii_case("BUTTON")
                }) {
                    let (parent, id) = (w.parent, w.id);
                    self.click_button(hwnd);
                    if parent != 0 {
                        self.post(parent, WM_COMMAND, id & 0xffff, hwnd);
                    }
                }
            }
            // Which buttons are held, for this message's MK_* flags.
            match message {
                0x0201 | 0x0203 => self.buttons |= 1,  // left down / double click
                0x0202 => self.buttons &= !1,
                0x0204 | 0x0206 => self.buttons |= 2,  // right
                0x0205 => self.buttons &= !2,
                0x0207 | 0x0209 => self.buttons |= 0x10, // middle
                0x0208 => self.buttons &= !0x10,
                _ => {}
            }
            let wparam = self.buttons
                | if self.key_down[0x10] { 4 } else { 0 } // MK_SHIFT
                | if self.key_down[0x11] { 8 } else { 0 }; // MK_CONTROL
            self.post(
                hwnd,
                message,
                wparam,
                ((cy as u32 & 0xffff) << 16) | (cx as u32 & 0xffff),
            );
        }
    }

    /// A left-button press is a plain WM_LBUTTONDOWN, or WM_LBUTTONDBLCLK when it is the
    /// second quick press on a window whose class asked for double clicks.
    fn classify_press(&mut self, hwnd: u32, x: i32, y: i32) -> u32 {
        let wants_dbl = self
            .window(hwnd)
            .and_then(|w| self.classes.get(&w.class.to_ascii_lowercase()))
            .is_some_and(|c| c.style & CS_DBLCLKS != 0);
        let now = self.now_ms;
        let is_double = wants_dbl
            && self.last_click.is_some_and(|(t, lx, ly, lh)| {
                lh == hwnd
                    && now.saturating_sub(t) <= DBLCLK_MS
                    && (x - lx).abs() <= DBLCLK_PX
                    && (y - ly).abs() <= DBLCLK_PX
            });
        if is_double {
            self.last_click = None; // a third press starts over
            WM_LBUTTONDBLCLK
        } else {
            self.last_click = Some((now, x, y, hwnd));
            WM_LBUTTONDOWN
        }
    }

    /// Apply a click's effect on a button's own state: auto check boxes toggle, auto radio
    /// buttons select themselves and deselect the rest of their group.
    fn click_button(&mut self, hwnd: u32) {
        const BS_AUTOCHECKBOX: u32 = 3;
        const BS_AUTORADIOBUTTON: u32 = 9;
        let Some(w) = self.window(hwnd) else { return };
        let (ty, parent) = (w.style & 0xF, w.parent);
        match ty {
            BS_AUTOCHECKBOX => {
                if let Some(w) = self.window_mut(hwnd) {
                    w.check = (w.check == 0) as u32;
                }
            }
            BS_AUTORADIOBUTTON => {
                for h in self.radio_group(parent, hwnd) {
                    if let Some(w) = self.window_mut(h) {
                        w.check = (h == hwnd) as u32;
                    }
                }
            }
            _ => {}
        }
    }

    /// The radio buttons that share a group with `hwnd`: consecutive siblings, bounded by
    /// the nearest WS_GROUP window at or before it and the next WS_GROUP window after it.
    pub fn radio_group(&self, parent: u32, hwnd: u32) -> Vec<u32> {
        const WS_GROUP: u32 = 0x0002_0000;
        let siblings: Vec<&Window> = self.windows.iter().filter(|w| w.parent == parent).collect();
        let Some(pos) = siblings.iter().position(|w| w.hwnd == hwnd) else {
            return vec![];
        };
        let start = (0..=pos)
            .rev()
            .find(|&i| siblings[i].style & WS_GROUP != 0)
            .unwrap_or(0);
        let end = (pos + 1..siblings.len())
            .find(|&i| siblings[i].style & WS_GROUP != 0)
            .unwrap_or(siblings.len());
        siblings[start..end]
            .iter()
            .filter(|w| matches!(w.style & 0xF, 4 | 9))
            .map(|w| w.hwnd)
            .collect()
    }

    pub fn post_char(&mut self, ch: u32) {
        let target = if self.focus != 0 {
            self.focus
        } else {
            self.windows
                .iter()
                .rev()
                .find(|w| w.parent == 0 && w.visible)
                .map(|w| w.hwnd)
                .unwrap_or(0)
        };
        if target != 0 {
            self.post(target, WM_CHAR, ch, 1);
        }
    }

    /// The host window's close button: ask the topmost top-level window to close.
    pub fn request_close(&mut self) {
        if let Some(hwnd) = self
            .windows
            .iter()
            .rev()
            .find(|w| w.parent == 0 && w.visible)
            .map(|w| w.hwnd)
        {
            self.post(hwnd, WM_CLOSE, 0, 0);
        } else {
            self.quit = Some(0);
        }
    }

    // ---- menus --------------------------------------------------------------------------

    pub fn new_menu(&mut self) -> u32 {
        let h = self.next_menu;
        self.next_menu += 4;
        self.menus.insert(h, Menu::default());
        h
    }

    /// Build a menu tree from a parsed template; returns the menu-bar handle.
    pub fn build_menu(&mut self, items: &[ParsedItem]) -> u32 {
        let h = self.new_menu();
        for it in items {
            let submenu = if it.children.is_empty() {
                0
            } else {
                self.build_menu(&it.children)
            };
            let item = MenuItem {
                text: it.text.clone(),
                id: it.id,
                flags: it.flags,
                submenu,
            };
            self.menus.get_mut(&h).unwrap().items.push(item);
        }
        h
    }

    pub fn load_menu_template(&mut self, bytes: &[u8]) -> Option<u32> {
        let items = parse_menu_template(bytes)?;
        Some(self.build_menu(&items))
    }

    /// Attach a menu bar, growing/shrinking the client area by the bar's height.
    pub fn set_menu(&mut self, hwnd: u32, menu: u32) {
        let Some(w) = self.window(hwnd) else { return };
        let (x, y, ow, oh) = (w.x, w.y, w.w, w.h);
        let had = w.menu != 0;
        self.window_mut(hwnd).unwrap().menu = menu;
        // Keep the outer size; only the client area changes.
        if had != (menu != 0) {
            let w = self.window_mut(hwnd).unwrap();
            let (b, c) = Self::chrome(w);
            let (cw, ch) = ((ow - 2 * b).max(0) as u32, (oh - c - 2 * b).max(0) as u32);
            let mut fresh = Framebuffer::new(cw, ch);
            fresh.fill_rect(0, 0, cw, ch, w.bg);
            fresh.blit(&w.surface, 0, 0);
            w.surface = fresh;
            w.needs_paint = true;
            let _ = (x, y);
        }
    }

    /// Find a menu item by command id (searching `root` and its dropdowns) or, with
    /// `by_position`, by index in `root`. Returns (menu handle, index).
    pub fn find_item(&self, root: u32, id: u32, by_position: bool) -> Option<(u32, usize)> {
        if by_position {
            let m = self.menus.get(&root)?;
            return ((id as usize) < m.items.len()).then_some((root, id as usize));
        }
        let mut stack = vec![root];
        while let Some(h) = stack.pop() {
            let Some(m) = self.menus.get(&h) else {
                continue;
            };
            for (i, it) in m.items.iter().enumerate() {
                if it.submenu != 0 {
                    stack.push(it.submenu);
                } else if it.id == id {
                    return Some((h, i));
                }
            }
        }
        None
    }

    fn ensure_font(&mut self) {
        self.font.get_or_insert_with(Font::load_system);
    }

    /// Returns true if the click was consumed by the menu system.
    fn handle_menu_click(&mut self, x: i32, y: i32) -> bool {
        self.ensure_font();
        let font = self.font.as_ref().unwrap();
        // 1. An open dropdown gets first refusal.
        if let Some(om) = &self.open_menu {
            let (hwnd, index) = (om.hwnd, om.index);
            let Some((ox, oy)) = self.client_origin(hwnd) else {
                self.open_menu = None;
                return true;
            };
            let bar = self.window(hwnd).and_then(|w| self.menus.get(&w.menu));
            if let Some(bar) = bar {
                let layout = bar_layout(font, bar);
                if let Some(item) = bar.items.get(index)
                    && let Some(sub) = self.menus.get(&item.submenu)
                {
                    let (pw, rows) = popup_layout(font, sub);
                    let (px, py) = (ox + layout[index].0, oy);
                    if x >= px && x < px + pw {
                        for (i, (y0, y1)) in rows.iter().enumerate() {
                            if y >= py + y0 && y < py + y1 {
                                let it = &sub.items[i];
                                if it.flags & (MF_GRAYED | MF_SEPARATOR) == 0 && it.submenu == 0 {
                                    let id = it.id;
                                    self.open_menu = None;
                                    self.post(hwnd, WM_COMMAND, id, 0);
                                }
                                return true;
                            }
                        }
                    }
                }
                // Clicking another bar title switches; anywhere else just closes.
                let bar_y = oy - MENU_H;
                if y >= bar_y && y < oy {
                    for (i, (x0, x1)) in layout.iter().enumerate() {
                        if x >= ox + x0 && x < ox + x1 {
                            return self.open_bar_item(hwnd, i);
                        }
                    }
                }
            }
            self.open_menu = None;
            return true;
        }
        // 2. Menu bar of any top-level window.
        let hits: Vec<(u32, usize)> = self
            .windows
            .iter()
            .filter(|w| w.parent == 0 && w.menu != 0 && w.visible)
            .filter_map(|w| {
                let (ox, oy) = self.client_origin(w.hwnd)?;
                let (cw, _) = self.client_size(w.hwnd)?;
                if y < oy - MENU_H || y >= oy || x < ox || x >= ox + cw {
                    return None;
                }
                let layout = bar_layout(font, self.menus.get(&w.menu)?);
                layout
                    .iter()
                    .position(|(x0, x1)| x >= ox + x0 && x < ox + x1)
                    .map(|i| (w.hwnd, i))
            })
            .collect();
        match hits.last() {
            Some(&(hwnd, i)) => self.open_bar_item(hwnd, i),
            None => false,
        }
    }

    fn open_bar_item(&mut self, hwnd: u32, index: usize) -> bool {
        let sub = self
            .window(hwnd)
            .and_then(|w| self.menus.get(&w.menu))
            .and_then(|m| m.items.get(index))
            .map(|i| i.submenu);
        match sub {
            Some(sub) if sub != 0 => {
                self.post(hwnd, WM_INITMENUPOPUP, sub, index as u32);
                self.open_menu = Some(OpenMenu { hwnd, index });
                true
            }
            Some(_) => {
                // A bar item with no dropdown acts as a command.
                let id = self
                    .window(hwnd)
                    .and_then(|w| self.menus.get(&w.menu))
                    .map(|m| m.items[index].id)
                    .unwrap_or(0);
                self.open_menu = None;
                self.post(hwnd, WM_COMMAND, id, 0);
                true
            }
            None => false,
        }
    }

    // ---- keyboard ---------------------------------------------------------------------

    pub fn post_key(&mut self, vk: u32, down: bool) {
        let extended = crate::gui::keyboard::scan_code(vk).is_some_and(|s| s.1);
        self.post_key_ex(vk, 0, extended, down);
    }

    /// A key went down or up. `side` is the left/right code of a modifier (VK_LSHIFT,
    /// VK_RCONTROL, ...) or 0; `extended` is the keyboard's extended-key flag. The message
    /// carries what Windows puts in lParam (scan code, previous state, transition), and is
    /// WM_SYSKEYDOWN/UP while Alt is held, or for Alt and F10 themselves.
    pub fn post_key_ex(&mut self, vk: u32, side: u32, extended: bool, down: bool) {
        let was_down = self.key_down.get(vk as usize).copied().unwrap_or(false);
        if side != 0 {
            self.key_down[side as usize & 0xFF] = down;
            // The generic modifier is down while either side is.
            let (l, r) = match vk {
                0x10 => (0xA0, 0xA1),
                0x11 => (0xA2, 0xA3),
                _ => (0xA4, 0xA5),
            };
            self.key_down[vk as usize & 0xFF] = self.key_down[l] || self.key_down[r];
        } else if let Some(k) = self.key_down.get_mut(vk as usize) {
            *k = down;
        }
        let alt = self.key_down[0x12];
        let sys = (alt && !self.key_down[0x11]) || vk == 0x12 || vk == 0x79;
        let message = match (down, sys) {
            (true, false) => WM_KEYDOWN,
            (false, false) => WM_KEYUP,
            (true, true) => WM_SYSKEYDOWN,
            (false, true) => WM_SYSKEYUP,
        };
        let scan_vk = if side != 0 { side } else { vk };
        let lparam = crate::gui::keyboard::key_lparam(scan_vk, extended, down, was_down, alt && sys);
        let target = if self.focus != 0 {
            self.focus
        } else {
            self.windows
                .iter()
                .rev()
                .find(|w| w.parent == 0 && w.visible)
                .map(|w| w.hwnd)
                .unwrap_or(0)
        };
        if target != 0 {
            self.post(target, message, vk, lparam);
        }
    }

    /// Does `accel` fire for this key, given the modifier keys currently held?
    pub fn accel_matches(&self, a: &Accel, vk: u32) -> bool {
        const FVIRTKEY: u8 = 1;
        const FSHIFT: u8 = 4;
        const FCONTROL: u8 = 8;
        const FALT: u8 = 0x10;
        let held = |code: usize| self.key_down[code];
        let mods_ok = (a.flags & FSHIFT != 0) == held(0x10)
            && (a.flags & FCONTROL != 0) == held(0x11)
            && (a.flags & FALT != 0) == held(0x12);
        a.flags & FVIRTKEY != 0 && a.key as u32 == vk && mods_ok
    }

    // ---- GDI objects --------------------------------------------------------------------

    pub fn add_gdi(&mut self, obj: Gdi) -> u32 {
        let h = self.next_gdi;
        self.next_gdi += 4;
        self.gdi.insert(h, obj);
        h
    }

    /// DeleteObject: stock objects are silently kept (like Windows), others are freed.
    pub fn delete_gdi(&mut self, h: u32) -> bool {
        if (0x8000_0000..0x8100_0000).contains(&h) {
            return true;
        }
        self.gdi.remove(&h).is_some()
    }

    pub fn new_dc(&mut self, target: Target) -> u32 {
        let bitmap = if target == Target::Mem {
            self.add_gdi(Gdi::Bitmap(Framebuffer::new(1, 1)))
        } else {
            0
        };
        self.add_gdi(Gdi::Dc(Dc {
            target,
            text_color: 0,
            bk_color: 0xFFFFFF,
            transparent: false,
            pen: stock::BLACK_PEN,
            brush: stock::WHITE_BRUSH,
            font: stock::FONT,
            bitmap,
            pos: (0, 0),
        }))
    }

    pub fn dc(&self, hdc: u32) -> Option<&Dc> {
        match self.gdi.get(&hdc) {
            Some(Gdi::Dc(d)) => Some(d),
            _ => None,
        }
    }

    pub fn dc_mut(&mut self, hdc: u32) -> Option<&mut Dc> {
        match self.gdi.get_mut(&hdc) {
            Some(Gdi::Dc(d)) => Some(d),
            _ => None,
        }
    }

    pub fn brush_color(&self, h: u32) -> Option<u32> {
        match self.gdi.get(&h) {
            Some(Gdi::Brush { color, null: false }) => Some(*color),
            _ => None,
        }
    }

    fn font_px(&self, h: u32) -> u32 {
        match self.gdi.get(&h) {
            Some(Gdi::Font { px }) => *px,
            _ => 13,
        }
    }

    pub fn measure_text(&mut self, hdc: u32, text: &str) -> (u32, u32) {
        let px = self.dc(hdc).map(|d| self.font_px(d.font)).unwrap_or(13);
        self.font
            .get_or_insert_with(Font::load_system)
            .measure(text, px)
    }

    /// Run `f` with the DC's drawing surface and a snapshot of its state.
    fn with_surface<R>(
        &mut self,
        hdc: u32,
        f: impl FnOnce(&mut Framebuffer, &Dc, &Resolved, &Font) -> R,
    ) -> Option<R> {
        self.font.get_or_insert_with(Font::load_system);
        let dc = self.dc(hdc)?.clone();
        let resolved = Resolved {
            pen: match self.gdi.get(&dc.pen) {
                Some(Gdi::Pen { color, width, null }) => Some((*color, *width)).filter(|_| !*null),
                _ => None,
            },
            brush: self.brush_color(dc.brush),
            px: self.font_px(dc.font),
        };
        let font = self.font.as_ref()?;
        let fb = match dc.target {
            Target::Window(h) => &mut self.windows.iter_mut().find(|w| w.hwnd == h)?.surface,
            Target::Mem => match self.gdi.get_mut(&dc.bitmap)? {
                Gdi::Bitmap(fb) => fb,
                _ => return None,
            },
        };
        Some(f(fb, &dc, &resolved, font))
    }

    pub fn fill_rect_dc(&mut self, hdc: u32, rect: (i32, i32, i32, i32), color: u32) {
        let (l, t, r, b) = rect;
        self.with_surface(hdc, |fb, _, _, _| {
            fb.fill_rect(l, t, (r - l).max(0) as u32, (b - t).max(0) as u32, color)
        });
    }

    /// Fill with the DC's current brush (PatBlt PATCOPY).
    pub fn brush_rect(&mut self, hdc: u32, rect: (i32, i32, i32, i32)) {
        let (l, t, r, b) = rect;
        self.with_surface(hdc, |fb, _, res, _| {
            if let Some(c) = res.brush {
                fb.fill_rect(l, t, (r - l).max(0) as u32, (b - t).max(0) as u32, c);
            }
        });
    }

    /// XOR every pixel in the rectangle with white (InvertRect / DSTINVERT).
    pub fn invert_rect(&mut self, hdc: u32, rect: (i32, i32, i32, i32)) {
        let (l, t, r, b) = rect;
        self.with_surface(hdc, |fb, _, _, _| {
            for y in t.max(0)..b.min(fb.height as i32) {
                for x in l.max(0)..r.min(fb.width as i32) {
                    let i = (y as u32 * fb.width + x as u32) as usize;
                    fb.pixels[i] ^= 0x00FF_FFFF;
                }
            }
        });
    }

    pub fn rectangle(&mut self, hdc: u32, rect: (i32, i32, i32, i32)) {
        let (l, t, r, b) = rect;
        self.with_surface(hdc, |fb, _, res, _| {
            let (w, h) = ((r - l).max(0) as u32, (b - t).max(0) as u32);
            if let Some(c) = res.brush {
                fb.fill_rect(l, t, w, h, c);
            }
            if let Some((c, _)) = res.pen {
                fb.frame_rect(l, t, w, h, c);
            }
        });
    }

    pub fn text_out(&mut self, hdc: u32, x: i32, y: i32, text: &str) {
        self.with_surface(hdc, |fb, dc, res, font| {
            if !dc.transparent {
                let (w, h) = font.measure(text, res.px);
                fb.fill_rect(x, y, w, h, dc.bk_color);
            }
            font.draw(fb, x, y, text, res.px, dc.text_color);
        });
    }

    pub fn line_to(&mut self, hdc: u32, x: i32, y: i32) {
        let from = self.dc(hdc).map(|d| d.pos);
        self.with_surface(hdc, |fb, _, res, _| {
            if let (Some((fx, fy)), Some((c, _))) = (from, res.pen) {
                fb.line(fx, fy, x, y, c);
            }
        });
        if let Some(d) = self.dc_mut(hdc) {
            d.pos = (x, y);
        }
    }

    pub fn set_pixel(&mut self, hdc: u32, x: i32, y: i32, color: u32) {
        self.with_surface(hdc, |fb, _, _, _| fb.set_pixel(x, y, color));
    }

    pub fn get_pixel(&mut self, hdc: u32, x: i32, y: i32) -> Option<u32> {
        self.with_surface(hdc, |fb, _, _, _| {
            if x >= 0 && y >= 0 {
                fb.get_pixel(x as u32, y as u32)
            } else {
                None
            }
        })
        .flatten()
    }

    /// StretchBlt: scale the source rectangle to the destination rectangle (SRCCOPY).
    #[allow(clippy::too_many_arguments)]
    pub fn stretch_blt(
        &mut self,
        dst: u32,
        dx: i32,
        dy: i32,
        dw: i32,
        dh: i32,
        src: u32,
        sx: i32,
        sy: i32,
        sw: i32,
        sh: i32,
    ) -> bool {
        if dw == 0 || dh == 0 || sw == 0 || sh == 0 {
            return false;
        }
        let Some(patch) = self.with_surface(src, |fb, _, _, _| {
            fb.crop(sx, sy, sw.unsigned_abs(), sh.unsigned_abs())
        }) else {
            return false;
        };
        let scaled = patch.scaled(dw.unsigned_abs(), dh.unsigned_abs());
        // A negative extent on exactly one side mirrors the image.
        let (flip_x, flip_y) = ((dw < 0) != (sw < 0), (dh < 0) != (sh < 0));
        let (w, h) = (scaled.width, scaled.height);
        let mut out = Framebuffer::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (
                    if flip_x { w - 1 - x } else { x },
                    if flip_y { h - 1 - y } else { y },
                );
                out.pixels[(y * w + x) as usize] = scaled.pixels[(fy * w + fx) as usize];
            }
        }
        let (ox, oy) = (
            if dw < 0 { dx + dw } else { dx },
            if dh < 0 { dy + dh } else { dy },
        );
        self.with_surface(dst, |fb, _, _, _| fb.blit(&out, ox, oy))
            .is_some()
    }

    /// Copy a rectangle from one DC to another (SRCCOPY).
    pub fn bit_blt(
        &mut self,
        dst: u32,
        dx: i32,
        dy: i32,
        w: u32,
        h: u32,
        src: u32,
        sx: i32,
        sy: i32,
    ) -> bool {
        let Some(patch) = self.with_surface(src, |fb, _, _, _| fb.crop(sx, sy, w, h)) else {
            return false;
        };
        self.with_surface(dst, |fb, _, _, _| fb.blit(&patch, dx, dy))
            .is_some()
    }

    // ---- compositor ---------------------------------------------------------------------

    /// Draw every visible window onto the desktop and return it.
    pub fn compose(&mut self) -> &Framebuffer {
        let font = self.font.take().unwrap_or_else(Font::load_system);
        // Built-in controls have no guest code to paint them, so we do it.
        for w in self.windows.iter_mut().filter(|w| w.wndproc == 0) {
            render_builtin(w, &font);
        }
        self.desktop
            .fill_rect(0, 0, DESKTOP_W, DESKTOP_H, DESKTOP_COLOR);
        let order: Vec<u32> = self.windows.iter().map(|w| w.hwnd).collect();
        for hwnd in order {
            if !self.is_visible(hwnd) {
                continue;
            }
            let Some((ox, oy)) = self.client_origin(hwnd) else {
                continue;
            };
            let Some(w) = self.windows.iter().find(|w| w.hwnd == hwnd) else {
                continue;
            };
            if w.parent == 0 {
                let (b, c) = Self::frame(w.style, false);
                if b > 0 {
                    let frame_color = if c > 0 { BTN_FACE } else { 0 };
                    self.desktop.fill_rect(
                        w.x,
                        w.y,
                        w.w.max(0) as u32,
                        w.h.max(0) as u32,
                        frame_color,
                    );
                }
                if c > 0 {
                    self.desktop.fill_rect(
                        w.x + b,
                        w.y + b,
                        (w.w - 2 * b).max(0) as u32,
                        c as u32,
                        CAPTION_COLOR,
                    );
                    font.draw(
                        &mut self.desktop,
                        w.x + b + 4,
                        w.y + b + 2,
                        &w.title,
                        12,
                        0xFFFFFF,
                    );
                }
            }
            self.desktop.blit(&w.surface, ox, oy);
            if w.parent == 0
                && w.menu != 0
                && let Some(menu) = self.menus.get(&w.menu)
            {
                let open = self
                    .open_menu
                    .as_ref()
                    .filter(|o| o.hwnd == hwnd)
                    .map(|o| o.index);
                let cw = w.surface.width as i32;
                draw_menu_bar(&mut self.desktop, &font, menu, ox, oy - MENU_H, cw, open);
            }
        }
        if let Some(om) = &self.open_menu
            && let (Some((ox, oy)), Some(w)) = (self.client_origin(om.hwnd), self.window(om.hwnd))
        {
            let bar = self.menus.get(&w.menu);
            let item = bar.and_then(|b| b.items.get(om.index));
            if let (Some(bar), Some(item)) = (bar, item)
                && let Some(sub) = self.menus.get(&item.submenu)
            {
                let x0 = bar_layout(&font, bar)[om.index].0;
                draw_popup(&mut self.desktop, &font, sub, ox + x0, oy);
            }
        }
        self.font = Some(font);
        &self.desktop
    }
}

/// A menu as read from a resource template, before handles are assigned.
pub struct ParsedItem {
    pub text: String,
    pub id: u32,
    pub flags: u32,
    pub children: Vec<ParsedItem>,
}

/// Parse an RT_MENU resource: classic MENUITEMTEMPLATE or extended MENUEX.
pub fn parse_menu_template(b: &[u8]) -> Option<Vec<ParsedItem>> {
    let u16_at = |o: usize| {
        b.get(o..o + 2)
            .map(|x| u16::from_le_bytes([x[0], x[1]]) as u32)
    };
    let u32_at = |o: usize| {
        b.get(o..o + 4)
            .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
    };
    let wstr = |o: usize| -> Option<(String, usize)> {
        let mut units = Vec::new();
        let mut p = o;
        loop {
            let c = u16_at(p)? as u16;
            p += 2;
            if c == 0 {
                return Some((String::from_utf16_lossy(&units), p));
            }
            units.push(c);
        }
    };
    let version = u16_at(0)?;
    let start = 4 + u16_at(2)? as usize;
    if version == 0 {
        fn items(
            b: &[u8],
            mut p: usize,
            depth: u32,
            rd: &dyn Fn(usize) -> Option<u32>,
            ws: &dyn Fn(usize) -> Option<(String, usize)>,
        ) -> Option<(Vec<ParsedItem>, usize)> {
            let mut out = Vec::new();
            if depth > 16 {
                return None;
            }
            loop {
                let flags = rd(p)?;
                p += 2;
                let (id, text, children);
                if flags & MF_POPUP != 0 {
                    let (t, np) = ws(p)?;
                    let (kids, np) = items(b, np, depth + 1, rd, ws)?;
                    (id, text, children, p) = (0, t, kids, np);
                } else {
                    id = rd(p)?;
                    let (t, np) = ws(p + 2)?;
                    (text, children, p) = (t, Vec::new(), np);
                }
                let sep = flags & MF_SEPARATOR != 0
                    || (id == 0 && text.is_empty() && flags & MF_POPUP == 0);
                out.push(ParsedItem {
                    text,
                    id,
                    flags: (flags & (MF_GRAYED | 2 | MF_CHECKED))
                        | if sep { MF_SEPARATOR } else { 0 },
                    children,
                });
                if flags & MF_END != 0 {
                    return Some((out, p));
                }
            }
        }
        return items(b, start, 0, &u16_at, &wstr).map(|r| r.0);
    }
    // MENUEX
    fn ex_items(b: &[u8], mut p: usize, depth: u32) -> Option<(Vec<ParsedItem>, usize)> {
        let rd32 = |o: usize| {
            b.get(o..o + 4)
                .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        };
        let rd16 = |o: usize| {
            b.get(o..o + 2)
                .map(|x| u16::from_le_bytes([x[0], x[1]]) as u32)
        };
        let mut out = Vec::new();
        if depth > 16 {
            return None;
        }
        loop {
            let (ty, state, id, wflags) = (rd32(p)?, rd32(p + 4)?, rd32(p + 8)?, rd16(p + 12)?);
            p += 14;
            let mut units = Vec::new();
            loop {
                let c = rd16(p)? as u16;
                p += 2;
                if c == 0 {
                    break;
                }
                units.push(c);
            }
            p = (p + 3) & !3;
            let mut children = Vec::new();
            if wflags & 1 != 0 {
                p += 4; // dwHelpId
                (children, p) = ex_items(b, p, depth + 1)?;
            }
            let mut flags = 0;
            if state & 3 != 0 {
                flags |= MF_GRAYED;
            }
            if state & 8 != 0 {
                flags |= MF_CHECKED;
            }
            if ty & MF_SEPARATOR != 0 {
                flags |= MF_SEPARATOR;
            }
            out.push(ParsedItem {
                text: String::from_utf16_lossy(&units),
                id,
                flags,
                children,
            });
            if wflags & 0x80 != 0 {
                return Some((out, p));
            }
        }
    }
    let _ = u32_at;
    ex_items(b, start, 0).map(|r| r.0)
}

fn menu_label(text: &str) -> (String, String) {
    let clean = text.replace('&', "");
    match clean.split_once('\t') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (clean, String::new()),
    }
}

/// Horizontal extents of each title in a menu bar, relative to the client's left edge.
pub fn bar_layout(font: &Font, menu: &Menu) -> Vec<(i32, i32)> {
    let mut x = 2;
    menu.items
        .iter()
        .map(|it| {
            let w = font.measure(&menu_label(&it.text).0, 12).0 as i32 + 16;
            let r = (x, x + w);
            x += w;
            r
        })
        .collect()
}

/// Width and per-item (top, bottom) rows of a dropdown, relative to its top-left.
pub fn popup_layout(font: &Font, menu: &Menu) -> (i32, Vec<(i32, i32)>) {
    let mut width = 120;
    let mut y = 2;
    let mut rows = Vec::new();
    for it in &menu.items {
        let (label, key) = menu_label(&it.text);
        width = width.max(font.measure(&label, 12).0 as i32 + font.measure(&key, 12).0 as i32 + 56);
        let h = if it.flags & MF_SEPARATOR != 0 { 8 } else { 18 };
        rows.push((y, y + h));
        y += h;
    }
    (width, rows)
}

fn draw_menu_bar(
    fb: &mut Framebuffer,
    font: &Font,
    menu: &Menu,
    x: i32,
    y: i32,
    w: i32,
    open: Option<usize>,
) {
    fb.fill_rect(x, y, w.max(0) as u32, MENU_H as u32, BTN_FACE);
    for (i, (it, (x0, x1))) in menu.items.iter().zip(bar_layout(font, menu)).enumerate() {
        let (fg, bg) = if open == Some(i) {
            (0xFFFFFF, 0x0A246A)
        } else {
            (0, BTN_FACE)
        };
        fb.fill_rect(x + x0, y, (x1 - x0) as u32, MENU_H as u32, bg);
        font.draw(fb, x + x0 + 8, y + 2, &menu_label(&it.text).0, 12, fg);
    }
}

fn draw_popup(fb: &mut Framebuffer, font: &Font, menu: &Menu, x: i32, y: i32) {
    let (w, rows) = popup_layout(font, menu);
    let h = rows.last().map_or(4, |r| r.1 + 2);
    fb.fill_rect(x, y, w as u32, h as u32, BTN_FACE);
    fb.frame_rect(x, y, w as u32, h as u32, 0x404040);
    for (it, (y0, y1)) in menu.items.iter().zip(rows) {
        if it.flags & MF_SEPARATOR != 0 {
            fb.fill_rect(x + 2, y + y0 + 3, (w - 4) as u32, 1, 0x808080);
            continue;
        }
        let color = if it.flags & MF_GRAYED != 0 {
            0x808080
        } else {
            0
        };
        let (label, key) = menu_label(&it.text);
        font.draw(fb, x + 22, y + y0 + 2, &label, 12, color);
        if !key.is_empty() {
            let kw = font.measure(&key, 12).0 as i32;
            font.draw(fb, x + w - kw - 10, y + y0 + 2, &key, 12, color);
        }
        if it.flags & MF_CHECKED != 0 {
            let (cx, cy) = (x + 6, y + y0 + (y1 - y0) / 2);
            fb.line(cx, cy, cx + 2, cy + 3, color);
            fb.line(cx + 2, cy + 3, cx + 7, cy - 3, color);
        }
    }
}

pub struct DlgItem {
    pub style: u32,
    pub ex_style: u32,
    pub x: i32,
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub id: u32,
    pub class: String,
    pub title: String,
}

pub struct DlgTemplate {
    pub style: u32,
    pub ex_style: u32,
    /// The template's position; dialogs are centred on their owner instead.
    #[allow(dead_code)]
    pub x: i32,
    #[allow(dead_code)]
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub title: String,
    pub items: Vec<DlgItem>,
}

/// Dialog units to pixels for the classic 8pt system font (4 DLU wide = 6 px, 8 DLU tall = 13 px).
pub fn dlu_x(v: i32) -> i32 {
    (v * 6 + 2) / 4
}

pub fn dlu_y(v: i32) -> i32 {
    (v * 13 + 4) / 8
}

/// Parse a DLGTEMPLATE or DLGTEMPLATEEX (RT_DIALOG resource).
pub fn parse_dialog_template(b: &[u8]) -> Option<DlgTemplate> {
    struct Cur<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl Cur<'_> {
        fn u16(&mut self) -> Option<u32> {
            let v = self
                .b
                .get(self.p..self.p + 2)
                .map(|x| u16::from_le_bytes([x[0], x[1]]) as u32)?;
            self.p += 2;
            Some(v)
        }
        fn i16(&mut self) -> Option<i32> {
            self.u16().map(|v| v as u16 as i16 as i32)
        }
        fn u32(&mut self) -> Option<u32> {
            let v = self
                .b
                .get(self.p..self.p + 4)
                .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))?;
            self.p += 4;
            Some(v)
        }
        fn wstr(&mut self) -> Option<String> {
            let mut units = Vec::new();
            loop {
                let c = self.u16()? as u16;
                if c == 0 {
                    return Some(String::from_utf16_lossy(&units));
                }
                units.push(c);
            }
        }
        fn align4(&mut self) {
            self.p = (self.p + 3) & !3;
        }
        /// An ordinal (0xFFFF, id) or a string; ordinals/empties become `String`s via `atom`.
        fn name_or_ordinal(&mut self, atom: fn(u32) -> String) -> Option<String> {
            let first = self
                .b
                .get(self.p..self.p + 2)
                .map(|x| u16::from_le_bytes([x[0], x[1]]) as u32)?;
            match first {
                0 => {
                    self.p += 2;
                    Some(String::new())
                }
                0xFFFF => {
                    self.p += 2;
                    Some(atom(self.u16()?))
                }
                _ => self.wstr(),
            }
        }
    }
    fn class_atom(a: u32) -> String {
        match a {
            0x80 => "BUTTON",
            0x81 => "EDIT",
            0x82 => "STATIC",
            0x83 => "LISTBOX",
            0x84 => "SCROLLBAR",
            0x85 => "COMBOBOX",
            _ => "STATIC",
        }
        .to_string()
    }
    fn no_text(_: u32) -> String {
        String::new()
    }

    let mut c = Cur { b, p: 0 };
    let extended = b.get(2..4) == Some(&[0xFF, 0xFF]) && b.get(0..2) == Some(&[1, 0]);
    let (style, ex_style, count, x, y, cx, cy);
    if extended {
        c.p = 4;
        c.u32()?; // helpID
        ex_style = c.u32()?;
        style = c.u32()?;
        count = c.u16()?;
        (x, y, cx, cy) = (c.i16()?, c.i16()?, c.i16()?, c.i16()?);
    } else {
        style = c.u32()?;
        ex_style = c.u32()?;
        count = c.u16()?;
        (x, y, cx, cy) = (c.i16()?, c.i16()?, c.i16()?, c.i16()?);
    }
    c.name_or_ordinal(no_text)?; // menu
    c.name_or_ordinal(class_atom)?; // window class
    let title = c.wstr()?;
    if style & 0x40 != 0 {
        // DS_SETFONT
        c.u16()?; // point size
        if extended {
            c.u16()?; // weight
            c.p += 2; // italic, charset
        }
        c.wstr()?;
    }
    let mut items = Vec::new();
    for _ in 0..count {
        c.align4();
        let item = if extended {
            c.u32()?; // helpID
            let ex = c.u32()?;
            let st = c.u32()?;
            let (ix, iy, icx, icy) = (c.i16()?, c.i16()?, c.i16()?, c.i16()?);
            let id = c.u32()?;
            (st, ex, ix, iy, icx, icy, id)
        } else {
            let st = c.u32()?;
            let ex = c.u32()?;
            let (ix, iy, icx, icy) = (c.i16()?, c.i16()?, c.i16()?, c.i16()?);
            let id = c.u16()?;
            (st, ex, ix, iy, icx, icy, id)
        };
        let class = c.name_or_ordinal(class_atom)?;
        let text = c.name_or_ordinal(no_text)?;
        let extra = c.u16()?;
        c.p += extra as usize;
        items.push(DlgItem {
            style: item.0,
            ex_style: item.1,
            x: item.2,
            y: item.3,
            cx: item.4,
            cy: item.5,
            id: item.6,
            class,
            title: text,
        });
    }
    Some(DlgTemplate {
        style,
        ex_style,
        x,
        y,
        cx,
        cy,
        title,
        items,
    })
}

pub struct Resolved {
    pub pen: Option<(u32, i32)>,
    pub brush: Option<u32>,
    pub px: u32,
}

/// Font size for built-in controls. The dialogs were laid out for an 8pt bitmap-style
/// font that is narrower than the TrueType font we have, so text is set a little smaller.
const CTRL_PX: u32 = 11;

/// Remove keyboard-shortcut markers: "&Draw" -> "Draw", "&&" -> "&".
pub fn clean_label(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Break `text` into lines no wider than `max_w` pixels (greedy, on spaces).
pub fn wrap_text(font: &Font, text: &str, px: u32, max_w: u32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if font.measure(&format!("{line} {word}"), px).0 <= max_w => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

/// Draw text in a `width` column starting at `x`, optionally centred vertically in
/// `height`. It stays on one line if it fits at the control size or a point or two
/// smaller (our font runs wider than the one the dialogs were laid out for); only then
/// does it wrap.
#[allow(clippy::too_many_arguments)]
fn draw_wrapped(
    fb: &mut Framebuffer,
    font: &Font,
    text: &str,
    x: i32,
    width: u32,
    height: u32,
    centre_v: bool,
    color: u32,
) {
    let width = width.max(1);
    let single = (CTRL_PX.saturating_sub(2)..=CTRL_PX)
        .rev()
        .find(|&px| font.measure(text, px).0 <= width);
    let (px, lines) = match single {
        Some(px) => (px, vec![text.to_string()]),
        None => (CTRL_PX - 1, wrap_text(font, text, CTRL_PX - 1, width)),
    };
    let line_h = font.measure("M", px).1 as i32;
    let total = line_h * lines.len() as i32;
    let mut y = if centre_v {
        (height as i32 - total) / 2
    } else {
        0
    };
    for line in &lines {
        font.draw(fb, x, y, line, px, color);
        y += line_h;
    }
}

fn render_builtin(w: &mut Window, font: &Font) {
    let title = clean_label(&w.title);
    let (cw, ch) = (w.surface.width, w.surface.height);
    w.surface.fill_rect(0, 0, cw, ch, w.bg);
    let class = w.class.to_ascii_uppercase();
    let text_color = if w.enabled { 0 } else { 0x808080 };
    match class.as_str() {
        "BUTTON" => match w.style & 0xF {
            // check boxes
            2 | 3 | 5 | 6 => {
                let top = (ch as i32 - 13) / 2;
                w.surface.fill_rect(0, top, 13, 13, 0xFFFFFF);
                w.surface.frame_rect(0, top, 13, 13, 0x808080);
                if w.check != 0 {
                    let c = if w.check == 2 { 0x808080 } else { 0 };
                    w.surface.line(3, top + 6, 5, top + 8, c);
                    w.surface.line(5, top + 8, 10, top + 3, c);
                    w.surface.line(3, top + 7, 5, top + 9, c);
                    w.surface.line(5, top + 9, 10, top + 4, c);
                }
                draw_wrapped(
                    &mut w.surface,
                    font,
                    &title,
                    18,
                    cw.saturating_sub(18),
                    ch,
                    true,
                    text_color,
                );
            }
            // radio buttons
            4 | 9 => {
                let top = (ch as i32 - 12) / 2;
                for y in 0..12i32 {
                    for x in 0..12i32 {
                        let d2 = (2 * x - 11).pow(2) + (2 * y - 11).pow(2); // distance^2 from centre, x4
                        if d2 <= 121 {
                            w.surface.set_pixel(
                                x,
                                top + y,
                                if d2 >= 90 { 0x808080 } else { 0xFFFFFF },
                            );
                        }
                        if w.check != 0 && d2 <= 18 {
                            w.surface.set_pixel(x, top + y, 0);
                        }
                    }
                }
                draw_wrapped(
                    &mut w.surface,
                    font,
                    &title,
                    18,
                    cw.saturating_sub(18),
                    ch,
                    true,
                    text_color,
                );
            }
            // group box
            7 => {
                let (tw, th) = font.measure(&title, CTRL_PX);
                let top = th as i32 / 2;
                w.surface
                    .frame_rect(0, top, cw, ch.saturating_sub(top as u32), 0x808080);
                w.surface.fill_rect(6, 0, tw + 4, th, w.bg);
                font.draw(&mut w.surface, 8, 0, &title, CTRL_PX, text_color);
            }
            // push buttons
            _ => {
                w.surface.frame_rect(0, 0, cw, ch, 0x000000);
                w.surface.fill_rect(1, 1, cw.saturating_sub(2), 1, 0xFFFFFF);
                w.surface.fill_rect(1, 1, 1, ch.saturating_sub(2), 0xFFFFFF);
                w.surface.fill_rect(
                    1,
                    ch.saturating_sub(2) as i32,
                    cw.saturating_sub(2),
                    1,
                    0x808080,
                );
                w.surface.fill_rect(
                    cw.saturating_sub(2) as i32,
                    1,
                    1,
                    ch.saturating_sub(2),
                    0x808080,
                );
                let (tw, th) = font.measure(&title, CTRL_PX);
                font.draw(
                    &mut w.surface,
                    (cw as i32 - tw as i32) / 2,
                    (ch as i32 - th as i32) / 2,
                    &title,
                    CTRL_PX,
                    text_color,
                );
            }
        },
        "EDIT" => {
            w.surface.frame_rect(0, 0, cw, ch, 0x808080);
            font.draw(&mut w.surface, 3, 2, &title, CTRL_PX, text_color);
        }
        _ => draw_wrapped(&mut w.surface, font, &title, 0, cw, ch, false, text_color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(wm: &mut WindowManager, x: u32, y: u32, w: u32, h: u32) -> u32 {
        let hwnd = wm.add_window("Test", "T", WS_CAPTION, 0, (x, y, w, h), 0, 0);
        wm.window_mut(hwnd).unwrap().visible = true;
        hwnd
    }

    #[test]
    fn client_geometry_accounts_for_frame() {
        let mut wm = WindowManager::new();
        let w = top(&mut wm, 100, 50, 200, 150);
        assert_eq!(wm.client_size(w), Some((200 - 6, 150 - 19 - 6)));
        assert_eq!(wm.client_origin(w), Some((103, 50 + 3 + 19)));
        let borderless = wm.add_window("Test", "", 0, 0, (0, 0, 80, 60), 0, 0);
        assert_eq!(wm.client_size(borderless), Some((80, 60)));
    }

    #[test]
    fn child_origin_is_relative_to_parent_client() {
        let mut wm = WindowManager::new();
        let p = top(&mut wm, 100, 100, 300, 200);
        let c = wm.add_window("BUTTON", "OK", WS_CHILD, 0, (10, 20, 50, 20), p, 1);
        assert_eq!(wm.client_origin(c), Some((103 + 10, 100 + 3 + 19 + 20)));
    }

    #[test]
    fn default_position_and_size() {
        let mut wm = WindowManager::new();
        let w = wm.add_window(
            "T",
            "",
            WS_CAPTION,
            0,
            (CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT),
            0,
            0,
        );
        let win = wm.window(w).unwrap();
        assert_eq!((win.w, win.h), (640, 480));
        assert!(win.x >= 40 && win.y >= 40);
    }

    #[test]
    fn message_priority_posted_quit_timer_paint() {
        let mut wm = WindowManager::new();
        let w = top(&mut wm, 0, 0, 100, 100);
        wm.set_timer(w, 7, 50, 0);
        wm.quit = Some(3);
        wm.post(w, WM_CHAR, b'x' as u32, 1);
        wm.now_ms = 100;
        assert_eq!(
            wm.next_message(true).unwrap().message,
            WM_CHAR,
            "posted first"
        );
        let q = wm.next_message(true).unwrap();
        assert_eq!((q.message, q.wparam), (WM_QUIT, 3), "then WM_QUIT");
        let t = wm.next_message(true).unwrap();
        assert_eq!((t.message, t.wparam), (WM_TIMER, 7), "then timers");
        assert_eq!(
            wm.next_message(true).unwrap().message,
            WM_PAINT,
            "paint is lowest priority"
        );
        wm.window_mut(w).unwrap().needs_paint = false;
        assert_eq!(wm.next_message(true), None, "nothing left -> app must wait");
    }

    #[test]
    fn peek_does_not_consume() {
        let mut wm = WindowManager::new();
        wm.post(5, WM_CHAR, 1, 0);
        assert!(wm.next_message(false).is_some());
        assert_eq!(wm.queue.len(), 1);
        assert!(wm.next_message(true).is_some());
        assert!(wm.queue.is_empty());
    }

    #[test]
    fn hidden_windows_get_no_paint() {
        let mut wm = WindowManager::new();
        let w = wm.add_window("T", "", 0, 0, (0, 0, 10, 10), 0, 0);
        assert_eq!(wm.next_message(true), None);
        wm.window_mut(w).unwrap().visible = true;
        assert_eq!(wm.next_message(true).unwrap().message, WM_PAINT);
    }

    #[test]
    fn timers_repeat_and_can_be_killed() {
        let mut wm = WindowManager::new();
        wm.set_timer(1, 1, 100, 0);
        wm.now_ms = 99;
        assert_eq!(wm.next_message(true), None);
        wm.now_ms = 100;
        assert_eq!(wm.next_message(true).unwrap().message, WM_TIMER);
        assert_eq!(
            wm.next_message(true),
            None,
            "rescheduled one interval later"
        );
        wm.now_ms = 200;
        assert!(wm.next_message(true).is_some());
        assert!(wm.kill_timer(1, 1));
        wm.now_ms = 1000;
        assert_eq!(wm.next_message(true), None);
        assert!(!wm.kill_timer(1, 1));
    }

    #[test]
    fn hit_test_finds_topmost_and_child() {
        let mut wm = WindowManager::new();
        let a = top(&mut wm, 0, 0, 300, 300);
        let b = top(&mut wm, 100, 100, 300, 300);
        let btn = wm.add_window(
            "BUTTON",
            "x",
            WS_CHILD | WS_VISIBLE,
            0,
            (5, 5, 40, 20),
            b,
            9,
        );
        wm.window_mut(btn).unwrap().visible = true;
        let (ox, oy) = wm.client_origin(b).unwrap();
        assert_eq!(
            wm.hit_test(ox + 10, oy + 10),
            Some((btn, 5, 5)),
            "child wins over its parent"
        );
        assert_eq!(
            wm.hit_test(ox + 100, oy + 100).map(|h| h.0),
            Some(b),
            "b is above a"
        );
        assert_eq!(wm.hit_test(110, 40).map(|h| h.0), Some(a));
        assert_eq!(wm.hit_test(1000, 700), None);
    }

    #[test]
    fn mouse_click_posts_message_with_client_coordinates_and_focuses() {
        let mut wm = WindowManager::new();
        let w = top(&mut wm, 10, 10, 200, 200);
        let (ox, oy) = wm.client_origin(w).unwrap();
        wm.post_mouse(WM_LBUTTONDOWN, ox + 7, oy + 9);
        let m = wm.queue.pop_front().unwrap();
        assert_eq!(
            (m.hwnd, m.message, m.lparam),
            (w, WM_LBUTTONDOWN, (9 << 16) | 7)
        );
        assert_eq!(wm.focus, w);
    }

    #[test]
    fn removing_a_window_removes_descendants_and_their_state() {
        let mut wm = WindowManager::new();
        let p = top(&mut wm, 0, 0, 100, 100);
        let c = wm.add_window("BUTTON", "", WS_CHILD, 0, (0, 0, 10, 10), p, 1);
        wm.set_timer(c, 1, 10, 0);
        wm.post(c, WM_CHAR, 0, 0);
        wm.focus = c;
        let gone = wm.remove_window(p);
        assert_eq!(gone, vec![p, c]);
        assert!(wm.windows.is_empty() && wm.timers.is_empty() && wm.queue.is_empty());
        assert_eq!(wm.focus, 0);
    }

    #[test]
    fn request_close_targets_topmost_window_or_quits() {
        let mut wm = WindowManager::new();
        wm.request_close();
        assert_eq!(wm.quit, Some(0));
        let w = top(&mut wm, 0, 0, 50, 50);
        wm.request_close();
        assert_eq!(wm.queue.pop_front().unwrap().message, WM_CLOSE);
        assert_eq!(wm.queue.len(), 0);
        let _ = w;
    }

    #[test]
    fn gdi_drawing_lands_on_window_surface() {
        let mut wm = WindowManager::new();
        let w = wm.add_window("T", "", 0, 0, (0, 0, 40, 40), 0, 0);
        let dc = wm.new_dc(Target::Window(w));
        wm.fill_rect_dc(dc, (5, 5, 15, 15), 0xFF0000);
        let s = &wm.window(w).unwrap().surface;
        assert_eq!(s.get_pixel(5, 5), Some(0xFF0000));
        assert_eq!(
            s.get_pixel(15, 15),
            Some(BTN_FACE),
            "right/bottom edge is exclusive"
        );
        wm.line_to(dc, 0, 0); // pos starts at (0,0): degenerate line, black pen
        assert_eq!(wm.window(w).unwrap().surface.get_pixel(0, 0), Some(0));
    }

    #[test]
    fn rectangle_uses_pen_and_brush() {
        let mut wm = WindowManager::new();
        let w = wm.add_window("T", "", 0, 0, (0, 0, 40, 40), 0, 0);
        let dc = wm.new_dc(Target::Window(w));
        let red = wm.add_gdi(Gdi::Brush {
            color: 0xFF0000,
            null: false,
        });
        wm.dc_mut(dc).unwrap().brush = red;
        wm.rectangle(dc, (10, 10, 20, 20));
        let s = &wm.window(w).unwrap().surface;
        assert_eq!(s.get_pixel(15, 15), Some(0xFF0000));
        assert_eq!(s.get_pixel(10, 15), Some(0), "black pen outline");
    }

    #[test]
    fn mem_dc_bit_blt_copies_pixels() {
        let mut wm = WindowManager::new();
        let w = wm.add_window("T", "", 0, 0, (0, 0, 40, 40), 0, 0);
        let screen = wm.new_dc(Target::Window(w));
        let mem = wm.new_dc(Target::Mem);
        let bmp = wm.add_gdi(Gdi::Bitmap(Framebuffer::new(8, 8)));
        wm.dc_mut(mem).unwrap().bitmap = bmp;
        wm.fill_rect_dc(mem, (0, 0, 8, 8), 0x00FF00);
        assert!(wm.bit_blt(screen, 20, 20, 8, 8, mem, 0, 0));
        assert_eq!(wm.get_pixel(screen, 23, 23), Some(0x00FF00));
        assert_eq!(wm.get_pixel(screen, 19, 19), Some(BTN_FACE));
    }

    #[test]
    fn invert_and_brush_rect() {
        let mut wm = WindowManager::new();
        let w = wm.add_window("T", "", 0, 0, (0, 0, 20, 20), 0, 0);
        let dc = wm.new_dc(Target::Window(w));
        wm.brush_rect(dc, (0, 0, 4, 4)); // default brush is white
        assert_eq!(wm.get_pixel(dc, 1, 1), Some(0xFFFFFF));
        wm.invert_rect(dc, (0, 0, 2, 2));
        assert_eq!(wm.get_pixel(dc, 1, 1), Some(0));
        assert_eq!(
            wm.get_pixel(dc, 3, 3),
            Some(0xFFFFFF),
            "outside the inverted rect"
        );
        wm.invert_rect(dc, (-5, -5, 100, 100)); // clipped, must not panic
    }

    #[test]
    fn stretch_blt_scales_and_mirrors() {
        let mut wm = WindowManager::new();
        let w = wm.add_window("T", "", 0, 0, (0, 0, 40, 40), 0, 0);
        let screen = wm.new_dc(Target::Window(w));
        let mem = wm.new_dc(Target::Mem);
        let mut src = Framebuffer::new(2, 1);
        src.set_pixel(0, 0, 0x111111);
        src.set_pixel(1, 0, 0x222222);
        let bmp = wm.add_gdi(Gdi::Bitmap(src));
        wm.dc_mut(mem).unwrap().bitmap = bmp;
        assert!(wm.stretch_blt(screen, 0, 0, 4, 2, mem, 0, 0, 2, 1));
        assert_eq!(wm.get_pixel(screen, 1, 1), Some(0x111111));
        assert_eq!(wm.get_pixel(screen, 3, 0), Some(0x222222));
        // Mirrored horizontally: negative destination width draws leftwards from dx.
        assert!(wm.stretch_blt(screen, 14, 10, -4, 2, mem, 0, 0, 2, 1));
        assert_eq!(wm.get_pixel(screen, 10, 10), Some(0x222222));
        assert_eq!(wm.get_pixel(screen, 13, 10), Some(0x111111));
        assert!(
            !wm.stretch_blt(screen, 0, 0, 0, 2, mem, 0, 0, 2, 1),
            "empty extent is a no-op"
        );
    }

    #[test]
    fn bounds_cover_visible_top_level_windows_only() {
        let mut wm = WindowManager::new();
        assert_eq!(wm.bounds(), None, "empty desktop");
        let a = top(&mut wm, 100, 50, 200, 150);
        assert_eq!(wm.bounds(), Some((100, 50, 200, 150)));
        let hidden = wm.add_window("T", "", 0, 0, (900, 700, 50, 50), 0, 0);
        let _ = hidden;
        assert_eq!(
            wm.bounds(),
            Some((100, 50, 200, 150)),
            "hidden windows do not count"
        );
        top(&mut wm, 250, 100, 300, 300);
        assert_eq!(wm.bounds(), Some((100, 50, 450, 350)), "union of both");
        wm.add_window("BUTTON", "", WS_CHILD, 0, (0, 0, 999, 999), a, 1);
        assert_eq!(
            wm.bounds(),
            Some((100, 50, 450, 350)),
            "children do not count"
        );
        top(&mut wm, 1000, 700, 500, 500);
        let (x, y, w, h) = wm.bounds().unwrap();
        assert!(
            x + w as i32 <= DESKTOP_W as i32 && y + h as i32 <= DESKTOP_H as i32,
            "clamped to desktop"
        );
    }

    #[test]
    fn content_bounds_exclude_frame_and_caption() {
        let mut wm = WindowManager::new();
        assert_eq!(wm.content_bounds(), None);
        let w = top(&mut wm, 100, 50, 200, 150);
        // 3px frame on each side, 19px caption.
        assert_eq!(wm.content_bounds(), Some((103, 72, 194, 125)));
        assert_eq!(
            wm.bounds(),
            Some((100, 50, 200, 150)),
            "outer bounds still include the frame"
        );
        let menu = wm.new_menu();
        wm.set_menu(w, menu);
        // The menu bar is part of the content: it starts where the caption ends, and the
        // client area below it is shorter by the bar's height.
        assert_eq!(wm.content_bounds(), Some((103, 72, 194, 125)));
        let (_, oy) = wm.client_origin(w).unwrap();
        assert_eq!(oy, 50 + 3 + 19 + MENU_H);
    }

    #[test]
    fn content_bounds_skip_hidden_windows_and_children() {
        let mut wm = WindowManager::new();
        let a = top(&mut wm, 10, 10, 100, 100);
        wm.add_window("T", "", WS_CAPTION, 0, (500, 500, 100, 100), 0, 0); // hidden
        wm.add_window("BUTTON", "", WS_CHILD, 0, (0, 0, 900, 900), a, 1);
        assert_eq!(wm.content_bounds(), Some((13, 32, 94, 75)));
    }

    #[test]
    fn labels_lose_mnemonic_markers() {
        assert_eq!(clean_label("&Draw"), "Draw");
        assert_eq!(clean_label("Out&line dragging"), "Outline dragging");
        assert_eq!(clean_label("Q&&A"), "Q&A");
        assert_eq!(clean_label("&"), "");
    }

    #[test]
    fn text_wraps_on_spaces_and_keeps_long_words() {
        let font = Font::none(); // fixed advance: 6px per char at 11px
        let lines = wrap_text(&font, "aaa bbb ccc", 11, 6 * 7);
        assert_eq!(lines, ["aaa bbb", "ccc"]);
        assert_eq!(
            wrap_text(&font, "abcdefghij", 11, 12),
            ["abcdefghij"],
            "an overlong word stays whole"
        );
        assert!(wrap_text(&font, "", 11, 100).is_empty());
    }

    #[test]
    fn dialog_template_parses_classic_layout() {
        // style=0x80C80080-ish: DS_SETFONT; 1 item: a BUTTON "OK" id 1
        let mut t: Vec<u8> = Vec::new();
        t.extend_from_slice(&0x90C8_0040u32.to_le_bytes()); // style (includes DS_SETFONT)
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes()); // one item
        for v in [10i16, 20, 100, 60] {
            t.extend_from_slice(&v.to_le_bytes());
        }
        t.extend_from_slice(&[0, 0]); // no menu
        t.extend_from_slice(&[0, 0]); // default class
        for c in "Hi".encode_utf16().chain([0]) {
            t.extend_from_slice(&c.to_le_bytes());
        }
        t.extend_from_slice(&8u16.to_le_bytes()); // font size
        for c in "Sans".encode_utf16().chain([0]) {
            t.extend_from_slice(&c.to_le_bytes());
        }
        while !t.len().is_multiple_of(4) {
            t.push(0);
        }
        t.extend_from_slice(&0x5001_0001u32.to_le_bytes()); // item style
        t.extend_from_slice(&0u32.to_le_bytes());
        for v in [4i16, 5, 30, 12] {
            t.extend_from_slice(&v.to_le_bytes());
        }
        t.extend_from_slice(&1u16.to_le_bytes()); // id
        t.extend_from_slice(&[0xFF, 0xFF, 0x80, 0x00]); // class: BUTTON atom
        for c in "OK".encode_utf16().chain([0]) {
            t.extend_from_slice(&c.to_le_bytes());
        }
        t.extend_from_slice(&0u16.to_le_bytes()); // no creation data
        let d = parse_dialog_template(&t).unwrap();
        assert_eq!(
            (d.title.as_str(), d.x, d.y, d.cx, d.cy),
            ("Hi", 10, 20, 100, 60)
        );
        assert_eq!(d.items.len(), 1);
        let it = &d.items[0];
        assert_eq!(
            (it.class.as_str(), it.title.as_str(), it.id, it.x, it.cx),
            ("BUTTON", "OK", 1, 4, 30)
        );
        assert!(
            parse_dialog_template(&t[..t.len() - 9]).is_none(),
            "truncated template is rejected"
        );
    }

    #[test]
    fn radio_buttons_select_exclusively_within_a_group() {
        let mut wm = WindowManager::new();
        let dlg = wm.add_window("#32770", "", WS_CAPTION, 0, (0, 0, 200, 200), 0, 0);
        let mk = |wm: &mut WindowManager, id, group: bool| {
            let style = WS_CHILD | WS_VISIBLE | 9 | if group { 0x0002_0000 } else { 0 };
            let h = wm.add_window("BUTTON", "r", style, 0, (0, 0, 50, 14), dlg, id);
            wm.window_mut(h).unwrap().visible = true;
            h
        };
        let (a, b, c) = (
            mk(&mut wm, 1, true),
            mk(&mut wm, 2, false),
            mk(&mut wm, 3, true),
        );
        wm.click_button(a);
        wm.click_button(b);
        assert_eq!(
            (wm.window(a).unwrap().check, wm.window(b).unwrap().check),
            (0, 1),
            "b replaced a"
        );
        wm.click_button(c);
        assert_eq!(
            wm.window(b).unwrap().check,
            1,
            "c starts a new group, so b is untouched"
        );
        assert_eq!(wm.window(c).unwrap().check, 1);
        assert_eq!(wm.radio_group(dlg, b), vec![a, b]);
    }

    #[test]
    fn auto_checkbox_toggles() {
        let mut wm = WindowManager::new();
        let dlg = wm.add_window("#32770", "", WS_CAPTION, 0, (0, 0, 100, 100), 0, 0);
        let cb = wm.add_window("BUTTON", "x", WS_CHILD | 3, 0, (0, 0, 50, 14), dlg, 5);
        wm.click_button(cb);
        assert_eq!(wm.window(cb).unwrap().check, 1);
        wm.click_button(cb);
        assert_eq!(wm.window(cb).unwrap().check, 0);
    }

    #[test]
    fn quick_second_press_becomes_a_double_click_only_for_dblclks_classes() {
        let mut wm = WindowManager::new();
        wm.classes.insert(
            "dbl".into(),
            WndClass {
                wndproc: 1,
                bg: 0,
                menu_name: 0,
                wide: false,
                style: CS_DBLCLKS,
            },
        );
        wm.classes.insert(
            "plain".into(),
            WndClass {
                wndproc: 1,
                bg: 0,
                menu_name: 0,
                wide: false,
                style: 0,
            },
        );
        let mk = |wm: &mut WindowManager, class: &str| {
            let h = wm.add_window(class, "", 0, 0, (0, 0, 200, 200), 0, 0);
            wm.window_mut(h).unwrap().visible = true;
            h
        };
        let dbl = mk(&mut wm, "dbl");
        let kinds =
            |wm: &mut WindowManager| wm.queue.drain(..).map(|m| m.message).collect::<Vec<_>>();

        wm.now_ms = 0;
        wm.post_mouse(WM_LBUTTONDOWN, 50, 50);
        wm.now_ms = 300;
        wm.post_mouse(WM_LBUTTONDOWN, 52, 49);
        assert_eq!(
            kinds(&mut wm),
            [WM_LBUTTONDOWN, WM_LBUTTONDBLCLK],
            "second quick press"
        );
        wm.now_ms = 350;
        wm.post_mouse(WM_LBUTTONDOWN, 52, 49);
        assert_eq!(
            kinds(&mut wm),
            [WM_LBUTTONDOWN],
            "a third press starts a new sequence"
        );

        wm.now_ms = 2000;
        wm.post_mouse(WM_LBUTTONDOWN, 50, 50);
        wm.now_ms = 2600;
        wm.post_mouse(WM_LBUTTONDOWN, 50, 50);
        assert_eq!(kinds(&mut wm), [WM_LBUTTONDOWN, WM_LBUTTONDOWN], "too slow");

        wm.now_ms = 10_000;
        wm.post_mouse(WM_LBUTTONDOWN, 50, 50);
        wm.now_ms = 10_100;
        wm.post_mouse(WM_LBUTTONDOWN, 90, 50);
        assert_eq!(
            kinds(&mut wm),
            [WM_LBUTTONDOWN, WM_LBUTTONDOWN],
            "too far apart"
        );

        wm.remove_window(dbl);
        let plain = mk(&mut wm, "plain");
        let _ = plain;
        wm.now_ms = 20_000;
        wm.post_mouse(WM_LBUTTONDOWN, 50, 50);
        wm.now_ms = 20_100;
        wm.post_mouse(WM_LBUTTONDOWN, 50, 50);
        assert_eq!(
            kinds(&mut wm),
            [WM_LBUTTONDOWN, WM_LBUTTONDOWN],
            "class did not ask for double clicks"
        );
    }

    #[test]
    fn colorref_swaps_red_and_blue() {
        assert_eq!(colorref(0x0000FF), 0xFF0000);
        assert_eq!(colorref(0x112233), 0x332211);
    }

    #[test]
    fn deleting_stock_objects_is_refused() {
        let mut wm = WindowManager::new();
        assert!(wm.delete_gdi(stock::WHITE_BRUSH));
        assert!(
            wm.gdi.contains_key(&stock::WHITE_BRUSH),
            "stock objects survive DeleteObject"
        );
        let b = wm.add_gdi(Gdi::Brush {
            color: 1,
            null: false,
        });
        assert!(wm.delete_gdi(b));
        assert!(!wm.gdi.contains_key(&b));
    }

    #[test]
    fn compose_draws_caption_client_and_children() {
        let mut wm = WindowManager::new();
        let w = top(&mut wm, 100, 100, 200, 150);
        wm.window_mut(w).unwrap().bg = 0x123456;
        wm.window_mut(w)
            .unwrap()
            .surface
            .fill_rect(0, 0, 500, 500, 0x123456);
        let btn = wm.add_window("BUTTON", "", WS_CHILD, 0, (10, 10, 60, 24), w, 1);
        wm.window_mut(btn).unwrap().visible = true;
        let fb = wm.compose();
        assert_eq!(
            fb.get_pixel(5, 5),
            Some(DESKTOP_COLOR),
            "desktop shows around windows"
        );
        assert_eq!(fb.get_pixel(110, 105), Some(CAPTION_COLOR), "caption bar");
        let (cx, cy) = (100 + 3 + 100, 100 + 3 + 19 + 100);
        assert_eq!(fb.get_pixel(cx, cy), Some(0x123456), "client area");
        assert_eq!(
            fb.get_pixel(103 + 10, 122 + 10),
            Some(0),
            "button's black outline"
        );
    }

    #[test]
    fn resize_keeps_existing_pixels_and_requests_repaint() {
        let mut wm = WindowManager::new();
        let w = wm.add_window("T", "", 0, 0, (0, 0, 20, 20), 0, 0);
        let dc = wm.new_dc(Target::Window(w));
        wm.fill_rect_dc(dc, (0, 0, 5, 5), 0xABCDEF);
        wm.window_mut(w).unwrap().needs_paint = false;
        wm.set_geometry(w, 0, 0, 40, 40);
        let win = wm.window(w).unwrap();
        assert_eq!((win.surface.width, win.surface.height), (40, 40));
        assert_eq!(win.surface.get_pixel(2, 2), Some(0xABCDEF));
        assert!(win.needs_paint);
    }
}
