//! Direct calls into AppKit for things minifb has no API for. minifb gives us the window's
//! NSView; everything goes through `objc_msgSend` cast to the exact signature of the
//! selector being invoked. NSSize/NSRect are two/four f64s, which arm64 passes and returns
//! in float registers, so these are only compiled for Apple Silicon.

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
        std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void, f64, f64, f64, f64)>(
            objc_msgSend as *const (),
        )(obj, sel, a, b, c, d)
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
        let point = std::mem::transmute::<*const (), unsafe extern "C" fn(Id, *mut c_void) -> Size>(
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
            obj, sel(c"setFrame:display:"), r.x, r.y, r.w, r.h, display
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
        let sup = Super {
            receiver: win,
            class: send_class(c"NSWindow"),
        };
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
        fn class_addMethod(
            cls: Id,
            name: *mut c_void,
            imp: *const (),
            types: *const c_char,
        ) -> bool;
    }
    // NSApplicationPresentationFullScreen | HideDock | HideMenuBar
    extern "C" fn options(_: Id, _: *mut c_void, _: Id, _: usize) -> usize {
        (1 << 10) | (1 << 1) | (1 << 3)
    }
    static DELEGATE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let Some((_, win)) = handles(window) else {
        return;
    };
    // SAFETY: see module docs; the class is made once, with a method of the signature
    // AppKit calls (NSApplicationPresentationOptions from (NSWindow*, options)).
    unsafe {
        let delegate = *DELEGATE.get_or_init(|| {
            let cls =
                objc_allocateClassPair(send_class(c"NSObject"), c"XPWindowDelegate".as_ptr(), 0);
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
    let (shift, control, option, command) = (
        f & 1 << 17 != 0,
        f & 1 << 18 != 0,
        f & 1 << 19 != 0,
        f & 1 << 20 != 0,
    );
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
    handles(window)
        .is_some_and(|(_, win)| unsafe { send_usize(win, sel(c"isKeyWindow")) } & 0xFF != 0)
}

/// Add an "Emulator" menu to the menu bar: Full Screen, and the master volume.
pub fn install_menu() {
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
        fn objc_registerClassPair(cls: Id);
        fn class_addMethod(
            cls: Id,
            name: *mut c_void,
            imp: *const (),
            types: *const c_char,
        ) -> bool;
    }
    extern "C" fn full_screen(_: Id, _: *mut c_void, _: Id) {
        super::FULLSCREEN_REQUEST.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    extern "C" fn volume(_: Id, _: *mut c_void, item: Id) {
        // SAFETY: AppKit passes the menu item that was chosen.
        unsafe {
            let percent = send_usize(item, sel(c"tag")) as u32;
            crate::media::speaker::MASTER_PERCENT
                .store(percent, std::sync::atomic::Ordering::Relaxed);
            let items = send_id(send_id(item, sel(c"menu")), sel(c"itemArray"));
            for i in 0..send_usize(items, sel(c"count")) {
                let other = send_id_index(items, sel(c"objectAtIndex:"), i);
                send_usize_arg(
                    other,
                    sel(c"setState:"),
                    (send_usize(other, sel(c"tag")) as u32 == percent) as usize,
                );
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
        class_addMethod(
            cls,
            sel(c"fullScreen:"),
            full_screen as *const (),
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            cls,
            sel(c"setVolume:"),
            volume as *const (),
            c"v@:@".as_ptr(),
        );
        objc_registerClassPair(cls);
        let target = send_id(send_id(cls, sel(c"alloc")), sel(c"init"));

        let string = |s: &std::ffi::CStr| {
            send_id_ptr(
                send_class(c"NSString"),
                sel(c"stringWithUTF8String:"),
                s.as_ptr() as *mut c_void,
            )
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
        let menu = send_id_ptr(
            send_id(send_class(c"NSMenu"), sel(c"alloc")),
            sel(c"initWithTitle:"),
            string(c"Emulator") as *mut c_void,
        );
        send_id_arg(
            menu,
            sel(c"addItem:"),
            item(c"Full Screen", sel(c"fullScreen:"), target),
        );
        send_id_arg(
            menu,
            sel(c"addItem:"),
            send_id(send_class(c"NSMenuItem"), sel(c"separatorItem")),
        );
        let header = item(c"Master Volume", std::ptr::null_mut(), std::ptr::null_mut());
        send_usize_arg(header, sel(c"setEnabled:"), 0);
        send_id_arg(menu, sel(c"addItem:"), header);
        let current =
            crate::media::speaker::MASTER_PERCENT.load(std::sync::atomic::Ordering::Relaxed);
        for (title, percent) in [
            (c"100%", 100),
            (c"75%", 75),
            (c"50%", 50),
            (c"25%", 25),
            (c"Mute", 0),
        ] {
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
        std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(Id, *mut c_void, Id, *mut c_void, Id) -> Id,
        >(objc_msgSend as *const ())(obj, sel, title, action, key)
    }
}

/// Is the window on the Space (virtual screen) being shown? The mouse position is global,
/// so on another Space it would still hover the game's buttons.
pub fn on_active_space(window: &Window) -> bool {
    // SAFETY: see module docs.
    handles(window)
        .is_none_or(|(_, win)| unsafe { send_usize(win, sel(c"isOnActiveSpace")) } & 0xFF != 0)
}

/// Debugging: which view a click at the window's centre would go to, and the frames of
/// the views on the way.
pub fn describe_hit(window: &Window) -> String {
    #[link(name = "objc")]
    unsafe extern "C" {
        fn object_getClassName(obj: Id) -> *const c_char;
    }
    let Some((view, win)) = handles(window) else {
        return "not attached".into();
    };
    // SAFETY: see module docs.
    unsafe {
        let name = |o: Id| {
            if o.is_null() {
                "nil".to_string()
            } else {
                std::ffi::CStr::from_ptr(object_getClassName(o))
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let content = send_id(win, sel(c"contentView"));
        let frame_view = send_id(content, sel(c"superview"));
        let top = send_id(frame_view, sel(c"superview"));
        let wf = send_rect(win, sel(c"frame"));
        let hit = std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(Id, *mut c_void, f64, f64) -> Id,
        >(objc_msgSend as *const ())(
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
            if top.is_null() {
                None
            } else {
                Some(send_rect(top, sel(c"frame")))
            },
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
        let reason = std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(Id, *mut c_void, *const c_char) -> Id,
        >(objc_msgSend as *const ())(
            send_class(c"NSString"),
            sel(c"stringWithUTF8String:"),
            c"Running a game".as_ptr(),
        );
        let info = send_id(send_class(c"NSProcessInfo"), sel(c"processInfo"));
        // NSActivityUserInitiated | NSActivityLatencyCritical
        let options: u64 = 0x00FF_FFFF | 0xFF_0000_0000;
        let token = std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(Id, *mut c_void, u64, Id) -> Id,
        >(objc_msgSend as *const ())(
            info,
            sel(c"beginActivityWithOptions:reason:"),
            options,
            reason,
        );
        send_id(token, sel(c"retain"));
    }
}

/// Is the window in macOS's own fullscreen?
pub fn in_native_fullscreen(window: &Window) -> bool {
    // SAFETY: see module docs.
    handles(window)
        .is_some_and(|(_, win)| unsafe { send_usize(win, sel(c"styleMask")) } & (1 << 14) != 0)
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
