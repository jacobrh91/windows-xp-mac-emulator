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

/// Direct calls into AppKit for things minifb has no API for. minifb gives us the window's
/// NSView; everything goes through `objc_msgSend` cast to the exact signature of the
/// selector being invoked. NSSize/NSRect are two/four f64s, which arm64 passes and returns
/// in float registers, so these are only compiled for Apple Silicon.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod cocoa {
    use std::ffi::{c_char, c_void};

    use minifb::{HasWindowHandle, Window};

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct Rect {
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        w: f64,
        h: f64,
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    type Id = *mut c_void;

    fn sel(name: &std::ffi::CStr) -> *mut c_void {
        // SAFETY: `name` is a valid NUL-terminated string.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// (NSView, NSWindow) for a minifb window, if it has been attached to a window yet.
    fn handles(window: &Window) -> Option<(Id, Id)> {
        let handle = window.window_handle().ok()?;
        let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        let view = appkit.ns_view.as_ptr();
        // SAFETY: `view` is the live NSView minifb created for this window.
        let ns_window = unsafe { send_id(view, sel(c"window")) };
        (!ns_window.is_null()).then_some((view, ns_window))
    }

    // SAFETY (all senders): `obj` must be a live Objective-C object that responds to `sel`
    // with the matching argument/return types. Each wrapper's types are fixed to one shape.
    unsafe fn send_id(obj: Id, sel: *mut c_void) -> Id {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void) -> Id>(
                objc_msgSend as *const (),
            )(obj, sel)
        }
    }

    unsafe fn send_usize(obj: Id, sel: *mut c_void) -> usize {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void) -> usize>(
                objc_msgSend as *const (),
            )(obj, sel)
        }
    }

    unsafe fn send_rect(obj: Id, sel: *mut c_void) -> Rect {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void) -> Rect>(
                objc_msgSend as *const (),
            )(obj, sel)
        }
    }

    unsafe fn send_2f(obj: Id, sel: *mut c_void, a: f64, b: f64) {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, f64, f64)>(
                objc_msgSend as *const (),
            )(obj, sel, a, b)
        }
    }

    unsafe fn send_4f(obj: Id, sel: *mut c_void, a: f64, b: f64, c: f64, d: f64) {
        unsafe {
            std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(Id, *mut c_void, f64, f64, f64, f64),
            >(objc_msgSend as *const ())(obj, sel, a, b, c, d)
        }
    }

    unsafe fn send_usize_arg(obj: Id, sel: *mut c_void, a: usize) {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, usize)>(
                objc_msgSend as *const (),
            )(obj, sel, a)
        }
    }

    unsafe fn send_id_arg(obj: Id, sel: *mut c_void, a: Id) {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, Id)>(
                objc_msgSend as *const (),
            )(obj, sel, a)
        }
    }

    /// Keep the content area at `w`:`h` while the user resizes. Returns whether it was
    /// applied and read back correctly.
    pub fn lock_aspect(window: &Window, w: u32, h: u32) -> bool {
        let Some((_, win)) = handles(window) else {
            return false;
        };
        // SAFETY: see module docs.
        unsafe {
            send_2f(win, sel(c"setContentAspectRatio:"), w as f64, h as f64);
            let s = std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void) -> Size>(
                objc_msgSend as *const (),
            )(win, sel(c"contentAspectRatio"));
            (s.w - w as f64).abs() < 0.5 && (s.h - h as f64).abs() < 0.5
        }
    }

    unsafe fn send_class(name: &std::ffi::CStr) -> Id {
        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_getClass(name: *const c_char) -> Id;
        }
        unsafe { objc_getClass(name.as_ptr()) }
    }

    /// minifb's custom window frame sizes its views as if the title bar did not exist, and
    /// entering/leaving fullscreen throws them off by a whole title bar (an empty strip
    /// above the game). So every frame: work out the area AppKit really shows, put the
    /// content views over exactly that, and place the drawing view in it. Windowed, it
    /// fills the area (the aspect lock keeps the proportions). Fullscreen, it keeps
    /// `aspect` and is centred on black. Returns None until the window is attached.
    pub fn layout(window: &Window, aspect: (u32, u32), fullscreen: bool) -> Option<Rect> {
        let (view, win) = handles(window)?;
        // SAFETY: see module docs.
        unsafe {
            let frame = send_rect(win, sel(c"frame"));
            // The title bar's height is what AppKit's own frame->content conversion removes.
            let titled_content = std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(Id, *mut c_void, f64, f64, f64, f64, usize) -> Rect,
            >(objc_msgSend as *const ())(
                send_class(c"NSWindow"),
                sel(c"contentRectForFrameRect:styleMask:"),
                frame.x,
                frame.y,
                frame.w,
                frame.h,
                1,
            );
            let titlebar = (frame.h - titled_content.h).max(0.0);
            let area = Rect {
                x: 0.0,
                y: 0.0,
                w: frame.w,
                h: if fullscreen {
                    frame.h
                } else {
                    frame.h - titlebar
                },
            };

            let target = if fullscreen && aspect.0 > 0 && aspect.1 > 0 {
                let scale = (area.w / aspect.0 as f64).min(area.h / aspect.1 as f64);
                let (tw, th) = (aspect.0 as f64 * scale, aspect.1 as f64 * scale);
                Rect {
                    x: ((area.w - tw) / 2.0).floor(),
                    y: ((area.h - th) / 2.0).floor(),
                    w: tw.floor(),
                    h: th.floor(),
                }
            } else {
                area
            };

            let content = send_id(win, sel(c"contentView"));
            set_frame_if_changed(content, area);
            set_frame_if_changed(view, target);
            // We lay the view out ourselves; stop AppKit's autoresizing fighting us.
            send_usize_arg(view, sel(c"setAutoresizingMask:"), 0);
            // Black around the picture (fullscreen letterbox).
            let black = send_id(send_class(c"NSColor"), sel(c"blackColor"));
            send_id_arg(win, sel(c"setBackgroundColor:"), black);
            Some(target) // where the picture sits, in the window's coordinates (origin bottom-left)
        }
    }

    unsafe fn set_frame_if_changed(view: Id, r: Rect) {
        unsafe {
            let cur = send_rect(view, sel(c"frame"));
            if (cur.x - r.x).abs() > 0.5
                || (cur.y - r.y).abs() > 0.5
                || (cur.w - r.w).abs() > 0.5
                || (cur.h - r.h).abs() > 0.5
            {
                send_4f(view, sel(c"setFrame:"), r.x, r.y, r.w, r.h);
            }
        }
    }

    /// Mouse position relative to the drawing view's top-left corner, plus the view's size,
    /// read from macOS directly (minifb's own mouse origin is off by the title bar).
    pub fn mouse_in_view(window: &Window) -> Option<((f64, f64), (f64, f64))> {
        let (view, win) = handles(window)?;
        // SAFETY: see module docs.
        unsafe {
            let point =
                std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void) -> Size>(
                    objc_msgSend as *const (),
                )(send_class(c"NSEvent"), sel(c"mouseLocation"));
            let frame = send_rect(win, sel(c"frame"));
            let v = send_rect(view, sel(c"frame"));
            let (lx, ly_bottom) = (point.w - frame.x - v.x, point.h - frame.y - v.y);
            Some(((lx, v.h - ly_bottom), (v.w, v.h)))
        }
    }

    /// What to restore when leaving fullscreen.
    pub struct Saved {
        frame: Rect,
        style_mask: usize,
    }

    unsafe fn send_set_frame_display(obj: Id, r: Rect, display: bool) {
        unsafe {
            std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(Id, *mut c_void, f64, f64, f64, f64, bool),
            >(objc_msgSend as *const ())(
                obj,
                sel(c"setFrame:display:"),
                r.x,
                r.y,
                r.w,
                r.h,
                display,
            )
        }
    }

    /// Fullscreen: show the menu bar over the game, or hide it again.
    pub fn show_menu_bar(show: bool) {
        // SAFETY: see module docs.
        unsafe { set_presentation_options(if show { 1 << 1 } else { HIDE_DOCK_AND_MENU_BAR }) }
    }

    unsafe fn set_presentation_options(options: usize) {
        unsafe {
            let app = send_id(send_class(c"NSApplication"), sel(c"sharedApplication"));
            send_usize_arg(app, sel(c"setPresentationOptions:"), options);
        }
    }

    /// minifb's window answers `contentView` with its inner view, while the real content view
    /// is the frame view around it, which is where mouse clicks are caught. Changing the
    /// style mask makes AppKit rebuild the window frame and re-install "the content view",
    /// so the frame view falls out and clicks go nowhere. These put it back.
    unsafe fn frame_view_of(win: Id) -> (Id, Id) {
        unsafe {
            let inner = send_id(win, sel(c"contentView"));
            let frame_view = send_id(inner, sel(c"superview"));
            // Kept alive across the style change (AppKit lets go of it).
            send_id(frame_view, sel(c"retain"));
            (frame_view, inner)
        }
    }

    unsafe fn restore_frame_view(win: Id, frame_view: Id, inner: Id) {
        #[repr(C)]
        struct Super {
            receiver: Id,
            class: Id,
        }
        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_msgSendSuper();
        }
        unsafe {
            if frame_view.is_null() || send_id(inner, sel(c"superview")) == frame_view {
                return;
            }
            // NSWindow's own setContentView:, not minifb's override (which would nest it).
            let sup = Super { receiver: win, class: send_class(c"NSWindow") };
            std::mem::transmute::<*const (), unsafe extern "C" fn(*const Super, *mut c_void, Id)>(
                objc_msgSendSuper as *const (),
            )(&sup, sel(c"setContentView:"), frame_view);
            if send_id(inner, sel(c"superview")) != frame_view {
                send_id_arg(frame_view, sel(c"addSubview:"), inner);
            }
        }
    }

    // NSApplicationPresentationHideDock | NSApplicationPresentationHideMenuBar
    const HIDE_DOCK_AND_MENU_BAR: usize = (1 << 1) | (1 << 3);
    // Just under the menu bar (NSMainMenuWindowLevel is 24), so that it can be shown over
    // the game.
    const COVER_LEVEL: usize = 23;

    /// Instant fullscreen: a borderless window over the whole screen with the Dock and menu
    /// bar hidden. macOS's own fullscreen plays a Space-switching animation that takes
    /// around a second each way, which is why this does it by hand. Returns what
    /// `exit_fullscreen` needs to put the window back.
    pub fn enter_fullscreen(window: &Window) -> Option<Saved> {
        let (_, win) = handles(window)?;
        // SAFETY: see module docs.
        unsafe {
            let saved = Saved {
                frame: send_rect(win, sel(c"frame")),
                style_mask: send_usize(win, sel(c"styleMask")),
            };
            let screen = send_id(win, sel(c"screen"));
            if screen.is_null() {
                return None;
            }
            let screen_frame = send_rect(screen, sel(c"frame"));
            set_presentation_options(HIDE_DOCK_AND_MENU_BAR);
            let (frame_view, inner) = frame_view_of(win);
            send_usize_arg(win, sel(c"setStyleMask:"), 0); // NSWindowStyleMaskBorderless
            restore_frame_view(win, frame_view, inner);
            send_usize_arg(win, sel(c"setLevel:"), COVER_LEVEL);
            send_set_frame_display(win, screen_frame, true);
            Some(saved)
        }
    }

    /// Make macOS's own fullscreen (the green button, Control-Command-F) completely full
    /// screen: the Dock and menu bar are hidden outright, so the game can scroll its map at
    /// the top edge.
    pub fn hide_menu_bar_in_fullscreen(window: &Window) {
        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
            fn objc_registerClassPair(cls: Id);
            fn class_addMethod(cls: Id, name: *mut c_void, imp: *const (), types: *const c_char) -> bool;
        }
        // NSApplicationPresentationFullScreen | HideDock | HideMenuBar
        extern "C" fn options(_: Id, _: *mut c_void, _: Id, _: usize) -> usize {
            (1 << 10) | (1 << 1) | (1 << 3)
        }
        static DELEGATE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let Some((_, win)) = handles(window) else { return };
        // SAFETY: see module docs; the class is made once, with a method of the signature
        // AppKit calls (NSApplicationPresentationOptions from (NSWindow*, options)).
        unsafe {
            let delegate = *DELEGATE.get_or_init(|| {
                let cls = objc_allocateClassPair(send_class(c"NSObject"), c"XPWindowDelegate".as_ptr(), 0);
                class_addMethod(
                    cls,
                    sel(c"window:willUseFullScreenPresentationOptions:"),
                    options as *const (),
                    c"Q@:@Q".as_ptr(),
                );
                objc_registerClassPair(cls);
                // One instance for every window, alive for the whole run (windows do not
                // retain their delegates).
                send_id(send_id(cls, sel(c"alloc")), sel(c"init")) as usize
            });
            send_id_arg(win, sel(c"setDelegate:"), delegate as Id);
        }
    }

    /// Debug: press the green button (macOS's own fullscreen).
    pub fn toggle_native_fullscreen(window: &Window) {
        if let Some((_, win)) = handles(window) {
            // SAFETY: see module docs.
            unsafe { send_id_arg(win, sel(c"toggleFullScreen:"), std::ptr::null_mut()) }
        }
    }

    /// The modifier keys held right now, left and right: [shift, shift, ctrl, ctrl, alt, alt]
    /// as a PC sees them (Control is Ctrl, Option is Alt), and whether Command is held.
    pub fn modifiers() -> Option<([bool; 6], bool)> {
        // SAFETY: see module docs; +[NSEvent modifierFlags] takes no arguments.
        let f = unsafe { send_usize(send_class(c"NSEvent"), sel(c"modifierFlags")) };
        // Device-dependent bits say which side (IOKit's NX_DEVICE*KEYMASK); the generic ones
        // (Shift 1<<17, Control 1<<18, Option 1<<19, Command 1<<20) back them up.
        let side = |bit: usize| f & bit != 0;
        let (shift, control, option, command) = (f & 1 << 17 != 0, f & 1 << 18 != 0, f & 1 << 19 != 0, f & 1 << 20 != 0);
        let (ls, rs) = (side(0x02), side(0x04));
        let (lc, rc) = (side(0x01), side(0x2000));
        let (la, ra) = (side(0x20), side(0x40));
        Some((
            [
                ls || (shift && !rs),
                rs,
                lc || (control && !rc),
                rc,
                la || (option && !ra),
                ra,
            ],
            command,
        ))
    }

    /// Is the window the one receiving the keyboard?
    pub fn is_key_window(window: &Window) -> bool {
        // SAFETY: see module docs.
        handles(window).is_some_and(|(_, win)| unsafe { send_usize(win, sel(c"isKeyWindow")) } & 0xFF != 0)
    }

    /// Add an "Emulator" menu to the menu bar: Full Screen, and the master volume.
    pub fn install_menu() {
        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
            fn objc_registerClassPair(cls: Id);
            fn class_addMethod(cls: Id, name: *mut c_void, imp: *const (), types: *const c_char) -> bool;
        }
        extern "C" fn full_screen(_: Id, _: *mut c_void, _: Id) {
            super::FULLSCREEN_REQUEST.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        extern "C" fn volume(_: Id, _: *mut c_void, item: Id) {
            // SAFETY: AppKit passes the menu item that was chosen.
            unsafe {
                let percent = send_usize(item, sel(c"tag")) as u32;
                crate::media::speaker::MASTER_PERCENT.store(percent, std::sync::atomic::Ordering::Relaxed);
                let items = send_id(send_id(item, sel(c"menu")), sel(c"itemArray"));
                for i in 0..send_usize(items, sel(c"count")) {
                    let other = send_id_index(items, sel(c"objectAtIndex:"), i);
                    send_usize_arg(other, sel(c"setState:"), (send_usize(other, sel(c"tag")) as u32 == percent) as usize);
                }
            }
        }
        // SAFETY: see module docs. The class has two action methods of the signature AppKit
        // calls (void, self, _cmd, sender); everything made here lives for the whole run.
        unsafe {
            let app = send_id(send_class(c"NSApplication"), sel(c"sharedApplication"));
            let bar = send_id(app, sel(c"mainMenu"));
            if bar.is_null() {
                return;
            }
            let cls = objc_allocateClassPair(send_class(c"NSObject"), c"XPMenuTarget".as_ptr(), 0);
            class_addMethod(cls, sel(c"fullScreen:"), full_screen as *const (), c"v@:@".as_ptr());
            class_addMethod(cls, sel(c"setVolume:"), volume as *const (), c"v@:@".as_ptr());
            objc_registerClassPair(cls);
            let target = send_id(send_id(cls, sel(c"alloc")), sel(c"init"));

            let string = |s: &std::ffi::CStr| {
                send_id_ptr(send_class(c"NSString"), sel(c"stringWithUTF8String:"), s.as_ptr() as *mut c_void)
            };
            let item = |title: &std::ffi::CStr, action: *mut c_void, to: Id| {
                let it = send_item_init(
                    send_id(send_class(c"NSMenuItem"), sel(c"alloc")),
                    sel(c"initWithTitle:action:keyEquivalent:"),
                    string(title),
                    action,
                    string(c""),
                );
                if !to.is_null() {
                    send_id_arg(it, sel(c"setTarget:"), to);
                }
                it
            };
            let menu = send_id_ptr(send_id(send_class(c"NSMenu"), sel(c"alloc")), sel(c"initWithTitle:"), string(c"Emulator") as *mut c_void);
            send_id_arg(menu, sel(c"addItem:"), item(c"Full Screen", sel(c"fullScreen:"), target));
            send_id_arg(menu, sel(c"addItem:"), send_id(send_class(c"NSMenuItem"), sel(c"separatorItem")));
            let header = item(c"Master Volume", std::ptr::null_mut(), std::ptr::null_mut());
            send_usize_arg(header, sel(c"setEnabled:"), 0);
            send_id_arg(menu, sel(c"addItem:"), header);
            let current = crate::media::speaker::MASTER_PERCENT.load(std::sync::atomic::Ordering::Relaxed);
            for (title, percent) in [(c"100%", 100), (c"75%", 75), (c"50%", 50), (c"25%", 25), (c"Mute", 0)] {
                let it = item(title, sel(c"setVolume:"), target);
                send_usize_arg(it, sel(c"setTag:"), percent as usize);
                send_usize_arg(it, sel(c"setState:"), (percent == current) as usize);
                send_usize_arg(it, sel(c"setIndentationLevel:"), 1);
                send_id_arg(menu, sel(c"addItem:"), it);
            }
            let top = item(c"Emulator", std::ptr::null_mut(), std::ptr::null_mut());
            send_id_arg(top, sel(c"setSubmenu:"), menu);
            // After the application menu.
            send_id_index_arg(bar, sel(c"insertItem:atIndex:"), top, 1);
        }
    }

    unsafe fn send_id_index(obj: Id, sel: *mut c_void, i: usize) -> Id {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, usize) -> Id>(
                objc_msgSend as *const (),
            )(obj, sel, i)
        }
    }

    unsafe fn send_id_index_arg(obj: Id, sel: *mut c_void, a: Id, i: usize) {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, Id, usize)>(
                objc_msgSend as *const (),
            )(obj, sel, a, i)
        }
    }

    unsafe fn send_id_ptr(obj: Id, sel: *mut c_void, a: *mut c_void) -> Id {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, *mut c_void) -> Id>(
                objc_msgSend as *const (),
            )(obj, sel, a)
        }
    }

    unsafe fn send_item_init(obj: Id, sel: *mut c_void, title: Id, action: *mut c_void, key: Id) -> Id {
        unsafe {
            std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, Id, *mut c_void, Id) -> Id>(
                objc_msgSend as *const (),
            )(obj, sel, title, action, key)
        }
    }

    /// Is the window on the Space (virtual screen) being shown? The mouse position is global,
    /// so on another Space it would still hover the game's buttons.
    pub fn on_active_space(window: &Window) -> bool {
        // SAFETY: see module docs.
        handles(window).is_none_or(|(_, win)| unsafe { send_usize(win, sel(c"isOnActiveSpace")) } & 0xFF != 0)
    }

    /// Debugging: which view a click at the window's centre would go to, and the frames of
    /// the views on the way.
    pub fn describe_hit(window: &Window) -> String {
        #[link(name = "objc")]
        unsafe extern "C" {
            fn object_getClassName(obj: Id) -> *const c_char;
        }
        let Some((view, win)) = handles(window) else { return "not attached".into() };
        // SAFETY: see module docs.
        unsafe {
            let name = |o: Id| if o.is_null() { "nil".to_string() } else { std::ffi::CStr::from_ptr(object_getClassName(o)).to_string_lossy().into_owned() };
            let content = send_id(win, sel(c"contentView"));
            let frame_view = send_id(content, sel(c"superview"));
            let top = send_id(frame_view, sel(c"superview"));
            let wf = send_rect(win, sel(c"frame"));
            let hit = std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, f64, f64) -> Id>(objc_msgSend as *const ())(
                if top.is_null() { frame_view } else { top },
                sel(c"hitTest:"),
                wf.w / 2.0,
                wf.h / 2.0,
            );
            format!(
                "hit at centre: {} | window {:?} | top {} {:?} | frameView {} {:?} | content {} {:?} | view {} {:?}",
                name(hit),
                wf,
                name(top),
                if top.is_null() { None } else { Some(send_rect(top, sel(c"frame"))) },
                name(frame_view),
                send_rect(frame_view, sel(c"frame")),
                name(content),
                send_rect(content, sel(c"frame")),
                name(view),
                send_rect(view, sel(c"frame")),
            )
        }
    }

    /// Keep macOS from napping the app (slowing its timers and threads) while its window is
    /// hidden, on another Space or behind others: the game and its sound must keep going.
    pub fn keep_awake() {
        // SAFETY: see module docs; the activity token is kept for the whole run.
        unsafe {
            let reason = std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, *const c_char) -> Id>(
                objc_msgSend as *const (),
            )(send_class(c"NSString"), sel(c"stringWithUTF8String:"), c"Running a game".as_ptr());
            let info = send_id(send_class(c"NSProcessInfo"), sel(c"processInfo"));
            // NSActivityUserInitiated | NSActivityLatencyCritical
            let options: u64 = 0x00FF_FFFF | 0xFF_0000_0000;
            let token = std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, u64, Id) -> Id>(
                objc_msgSend as *const (),
            )(info, sel(c"beginActivityWithOptions:reason:"), options, reason);
            send_id(token, sel(c"retain"));
        }
    }

    /// Is the window in macOS's own fullscreen?
    pub fn in_native_fullscreen(window: &Window) -> bool {
        // SAFETY: see module docs.
        handles(window).is_some_and(|(_, win)| unsafe { send_usize(win, sel(c"styleMask")) } & (1 << 14) != 0)
    }

    /// Debugging: the presentation options asked for and in force, the window's style mask,
    /// and whether the window is key and main and the app active.
    pub fn presentation_state(window: &Window) -> Option<String> {
        let (_, win) = handles(window)?;
        // SAFETY: see module docs.
        unsafe {
            let app = send_id(send_class(c"NSApplication"), sel(c"sharedApplication"));
            let flag = |obj: Id, s: &std::ffi::CStr| send_usize(obj, sel(s)) & 0xFF != 0;
            Some(format!(
                "options asked {:#x} in force {:#x} style {:#x} key {} main {} app active {}",
                send_usize(app, sel(c"presentationOptions")),
                send_usize(app, sel(c"currentSystemPresentationOptions")),
                send_usize(win, sel(c"styleMask")),
                flag(win, c"isKeyWindow"),
                flag(win, c"isMainWindow"),
                flag(app, c"isActive"),
            ))
        }
    }

    pub fn exit_fullscreen(window: &Window, saved: &Saved) {
        let Some((_, win)) = handles(window) else {
            return;
        };
        // SAFETY: see module docs.
        unsafe {
            send_usize_arg(win, sel(c"setLevel:"), 0);
            let (frame_view, inner) = frame_view_of(win);
            send_usize_arg(win, sel(c"setStyleMask:"), saved.style_mask);
            restore_frame_view(win, frame_view, inner);
            send_set_frame_display(win, saved.frame, true);
            set_presentation_options(0);
        }
    }

    /// Debug: resize the window's content area, as if the user had dragged its edge.
    pub fn debug_resize(window: &Window, w: f64, h: f64) {
        let Some((_, win)) = handles(window) else {
            return;
        };
        // SAFETY: see module docs.
        unsafe { send_2f(win, sel(c"setContentSize:"), w, h) }
    }

    /// Debug: NSWindow frame, content view frame, drawing view frame, screen frame.
    pub fn describe(window: &Window) -> String {
        let Some((view, win)) = handles(window) else {
            return "not attached".into();
        };
        // SAFETY: see module docs.
        unsafe {
            let wf = send_rect(win, sel(c"frame"));
            let cf = send_rect(send_id(win, sel(c"contentView")), sel(c"frame"));
            let vf = send_rect(view, sel(c"frame"));
            let screen = send_id(win, sel(c"screen"));
            let sf = if screen.is_null() {
                None
            } else {
                Some(send_rect(screen, sel(c"frame")))
            };
            format!("window={wf:?} content={cf:?} drawView={vf:?} screen={sf:?}")
        }
    }
}

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
        let title = emu.wm.windows.iter().find(|w| w.parent == 0 && w.visible).map(|w| w.title.clone());
        let fb = emu.wm.compose();
        let (rx, ry, rw, rh) = region.unwrap_or((0, 0, 0, 0));
        let mut pixels = Vec::with_capacity((rw * rh) as usize);
        for y in 0..rh {
            let row = ((ry as u32 + y) * fb.width + rx as u32) as usize;
            pixels.extend_from_slice(&fb.pixels[row..row + rw as usize]);
        }
        *self.frame.lock().unwrap() = Some(Frame { pixels, region, title });
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
                Input::Close => closed = true,
            }
        }
        closed
    }
}

/// The Emulator menu's Full Screen was chosen.
static FULLSCREEN_REQUEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
                let (vk, side, ext) = [(0x10, 0xA0, false), (0x10, 0xA1, false), (0x11, 0xA2, false), (0x11, 0xA3, true), (0x12, 0xA4, false), (0x12, 0xA5, true)][i];
                self.out.push(Input::Key(vk, side, ext, is));
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
            let _ = self.window.update_with_buffer(&self.shown, rw as usize, rh as usize);
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
            let b = [MouseButton::Left, MouseButton::Right, MouseButton::Middle].map(|m| self.window.get_mouse_down(m));
            if b != self.logged_buttons {
                self.logged_buttons = b;
                eprintln!(
                    "[input] buttons {b:?} game pos {mouse:?} view {:?} fullscreen {was_fullscreen}",
                    cocoa::mouse_in_view(&self.window).map(|((x, y), (w, h))| (x as i32, y as i32, w as i32, h as i32))
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
                    self.out.push(Input::Mouse(if down { WM_LBUTTONDOWN } else { WM_LBUTTONUP }, pos.0, pos.1));
                }
            }
            // Right and middle buttons (WM_RBUTTONDOWN/UP, WM_MBUTTONDOWN/UP).
            for (i, button, msg) in [(0, MouseButton::Right, 0x0204), (1, MouseButton::Middle, 0x0207)] {
                let down = self.window.get_mouse_down(button);
                if down != self.other_down[i] {
                    self.other_down[i] = down;
                    self.out.push(Input::Mouse(if down { msg } else { msg + 1 }, pos.0, pos.1));
                }
            }
        } else if self.left_down && !self.window.get_mouse_down(MouseButton::Left) {
            self.left_down = false; // released outside the picture: don't leave a drag stuck
            let up = if std::mem::take(&mut self.ctrl_click) { 0x0205 } else { WM_LBUTTONUP };
            self.out.push(Input::Mouse(up, self.last_pos.0, self.last_pos.1));
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
            eprintln!("[present] frame {} fullscreen={} {:?} modifiers {:?} sent {:?}", self.frames, was_fullscreen, cocoa::presentation_state(&self.window), cocoa::modifiers(), self.modifiers);
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
                    self.out.push(Input::Char(l.to_ascii_uppercase() as u32 & 0x1F));
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
