//! Tests of `wm`.

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
    let kinds = |wm: &mut WindowManager| wm.queue.drain(..).map(|m| m.message).collect::<Vec<_>>();

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
