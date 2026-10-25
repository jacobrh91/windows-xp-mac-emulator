//! USER32 and GDI32 on top of `wm::WindowManager`. Window procedures are guest code, so
//! sending a message means running the guest's WndProc via `Emu::call_function`.

use crate::cpu::{Emu, Res, Stop};
use crate::gui::wm::*;
use crate::win32::{Args, Handler};

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    // These take no strings (or only ids), so the W entry points are the A ones.
    const SAME_AS_A: &[&str] = &[
        "GetMessage",
        "PeekMessage",
        "DispatchMessage",
        "PostMessage",
        "SendMessage",
        "DefWindowProc",
        "LoadCursor",
        "LoadIcon",
    ];
    if let Some(base) = name.strip_suffix('W')
        && SAME_AS_A.contains(&base)
    {
        return lookup(dll, &format!("{base}A"));
    }
    let r: (usize, Handler) = match (dll, name) {
        // ---- window classes & lifetime ----
        ("user32", "RegisterClassA") => (1, |e, a| register_class(e, a[0], false, false)),
        ("user32", "RegisterClassExA") => (1, |e, a| register_class(e, a[0], true, false)),
        ("user32", "RegisterClassW") => (1, |e, a| register_class(e, a[0], false, true)),
        ("user32", "RegisterClassExW") => (1, |e, a| register_class(e, a[0], true, true)),
        ("user32", "CreateWindowExW") => (12, |e, a| create_window(e, a, true)),
        ("user32", "UnregisterClassA") => (2, |_, _| Ok(1)),
        ("user32", "CreateWindowExA") => (12, |e, a| create_window(e, a, false)),
        ("user32", "DestroyWindow") => (1, |e, a| {
            if e.wm.window(a[0]).is_none() {
                return Ok(0);
            }
            destroy_window(e, a[0])?;
            Ok(1)
        }),
        ("user32", "ShowWindow") => (2, |e, a| {
            let was = show_window(e, a[0], a[1] != 0)?;
            // Showing activates, except SW_SHOWMINNOACTIVE, SW_SHOWNA and SW_SHOWNOACTIVATE.
            if a[1] != 0 && !matches!(a[1], 4 | 7 | 8) {
                activate_top_level(e, a[0])?;
            }
            Ok(was)
        }),
        ("user32", "UpdateWindow") => (1, |e, a| {
            let due = e.wm.window(a[0]).is_some_and(|w| w.needs_paint) && e.wm.is_visible(a[0]);
            if due {
                send_message(e, a[0], WM_PAINT, 0, 0)?;
                if let Some(w) = e.wm.window_mut(a[0]) {
                    w.needs_paint = false;
                }
            }
            Ok(1)
        }),
        ("user32", "IsWindow") => (1, |e, a| Ok(e.wm.window(a[0]).is_some() as u32)),
        ("user32", "IsWindowVisible") => (1, |e, a| Ok(e.wm.is_visible(a[0]) as u32)),
        ("user32", "EnableWindow") => (2, |e, a| {
            let Some(w) = e.wm.window_mut(a[0]) else {
                return Ok(0);
            };
            let was_disabled = !w.enabled;
            w.enabled = a[1] != 0;
            Ok(was_disabled as u32)
        }),

        // ---- geometry ----
        ("user32", "MoveWindow") => (6, |e, a| {
            move_window(e, a[0], a[1] as i32, a[2] as i32, a[3] as i32, a[4] as i32)?;
            Ok(1)
        }),
        ("user32", "SetWindowPos") => (7, set_window_pos),
        ("user32", "GetClientRect") => (2, |e, a| {
            let Some((w, h)) = e.wm.client_size(a[0]) else {
                return Ok(0);
            };
            put_rect(e, a[1], (0, 0, w, h))?;
            Ok(1)
        }),
        ("user32", "GetWindowRect") => (2, |e, a| {
            let Some(w) = e.wm.window(a[0]) else {
                return Ok(0);
            };
            let (ox, oy) = match w.parent {
                0 => (0, 0),
                p => e.wm.client_origin(p).unwrap_or((0, 0)),
            };
            let r = (ox + w.x, oy + w.y, ox + w.x + w.w, oy + w.y + w.h);
            put_rect(e, a[1], r)?;
            Ok(1)
        }),
        ("user32", "AdjustWindowRectEx") => (4, |e, a| {
            let (l, t, r, b) = get_rect(e, a[0])?;
            let (bw, cap) = if a[1] & WS_CAPTION == WS_CAPTION {
                (3, 19)
            } else if a[1] & WS_BORDER != 0 {
                (1, 0)
            } else {
                (0, 0)
            };
            let cap = cap + if a[2] != 0 { MENU_H } else { 0 };
            put_rect(e, a[0], (l - bw, t - bw - cap, r + bw, b + bw))?;
            Ok(1)
        }),
        ("user32", "ClientToScreen") => (2, |e, a| shift_point(e, a[0], a[1], 1)),
        ("user32", "ScreenToClient") => (2, |e, a| shift_point(e, a[0], a[1], -1)),
        ("user32", "GetCursorPos") => (1, |e, a| {
            e.mem.write(a[0], 4, 0)?;
            e.mem.write(a[0] + 4, 4, 0)?;
            Ok(1)
        }),

        // ---- window data ----
        ("user32", "SetWindowTextA") => (2, |e, a| {
            let t = e.mem.read_cstr(a[1])?;
            Ok(e.wm.window_mut(a[0]).map(|w| w.title = t).is_some() as u32)
        }),
        ("user32", "GetWindowTextA") => (3, |e, a| {
            let Some(t) = e.wm.window(a[0]).map(|w| w.title.clone()) else {
                return Ok(0);
            };
            copy_text(e, a[1], a[2], &t)
        }),
        ("user32", "GetWindowLongA") => (2, |e, a| Ok(get_window_long(e, a[0], a[1] as i32))),
        ("user32", "SetWindowLongA") => (3, |e, a| {
            let old = get_window_long(e, a[0], a[1] as i32);
            if let Some(w) = e.wm.window_mut(a[0]) {
                match a[1] as i32 {
                    -4 => w.wndproc = a[2],
                    -16 => w.style = a[2],
                    -20 => w.ex_style = a[2],
                    -21 => w.userdata = a[2],
                    _ => {}
                }
            }
            Ok(old)
        }),
        ("user32", "GetParent") => (1, |e, a| Ok(e.wm.window(a[0]).map_or(0, |w| w.parent))),
        ("user32", "GetDlgItem") => (2, |e, a| {
            Ok(e.wm
                .windows
                .iter()
                .find(|w| w.parent == a[0] && w.id == a[1])
                .map_or(0, |w| w.hwnd))
        }),
        ("user32", "SetFocus") => (1, |e, a| {
            let old = std::mem::replace(&mut e.wm.focus, a[0]);
            if old != a[0] && e.wm.window(a[0]).is_some() {
                e.wm.post(a[0], 7, old, 0); // WM_SETFOCUS
            }
            Ok(old)
        }),
        ("user32", "GetFocus") => (0, |e, _| Ok(e.wm.focus)),
        ("user32", "GetActiveWindow") | ("user32", "GetForegroundWindow") => (0, |e, _| {
            if e.wm.window(e.wm.active).is_some() {
                return Ok(e.wm.active);
            }
            Ok(e.wm
                .windows
                .iter()
                .rev()
                .find(|w| w.parent == 0 && w.visible)
                .map_or(0, |w| w.hwnd))
        }),
        ("user32", "SetActiveWindow") | ("user32", "SetForegroundWindow") => (1, |e, a| {
            activate(e, a[0])?;
            Ok(1)
        }),
        ("user32", "SetCapture") => (1, |e, a| Ok(std::mem::replace(&mut e.wm.capture, a[0]))),
        ("user32", "ReleaseCapture") => (0, |e, _| {
            e.wm.capture = 0;
            Ok(1)
        }),

        // ---- messages ----
        ("user32", "GetMessageA") => (4, get_message),
        ("user32", "PeekMessageA") => (5, peek_message),
        ("user32", "TranslateMessage") => (1, |_, _| Ok(0)),
        ("user32", "DispatchMessageA") => (1, dispatch_message),
        ("user32", "PostMessageA") => (4, |e, a| {
            e.wm.post(a[0], a[1], a[2], a[3]);
            Ok(1)
        }),
        ("user32", "SendMessageA") => (4, |e, a| send_message(e, a[0], a[1], a[2], a[3])),
        ("user32", "PostQuitMessage") => (1, |e, a| {
            e.wm.quit = Some(a[0]);
            Ok(0)
        }),
        ("user32", "DefWindowProcA") => (4, |e, a| default_proc(e, a[0], a[1], a[2], a[3])),
        ("user32", "SetTimer") => (4, |e, a| Ok(e.wm.set_timer(a[0], a[1], a[2], a[3]))),
        ("user32", "KillTimer") => (2, |e, a| Ok(e.wm.kill_timer(a[0], a[1]) as u32)),

        // ---- painting ----
        ("user32", "InvalidateRect") => (3, |e, a| {
            for w in
                e.wm.windows
                    .iter_mut()
                    .filter(|w| a[0] == 0 || w.hwnd == a[0])
            {
                w.needs_paint = true;
            }
            Ok(1)
        }),
        ("user32", "ValidateRect") => (2, |e, a| {
            if let Some(w) = e.wm.window_mut(a[0]) {
                w.needs_paint = false;
            }
            Ok(1)
        }),
        ("user32", "GetDC") | ("user32", "GetWindowDC") => (1, |e, a| {
            let target = if a[0] == 0 {
                Target::Mem
            } else {
                Target::Window(a[0])
            };
            Ok(e.wm.new_dc(target))
        }),
        ("user32", "ReleaseDC") => (2, |e, a| Ok(e.wm.gdi.remove(&a[1]).is_some() as u32)),
        ("user32", "BeginPaint") => (2, begin_paint),
        ("user32", "EndPaint") => (2, |e, a| {
            let hdc = e.mem.read(a[1], 4)?;
            e.wm.gdi.remove(&hdc);
            Ok(1)
        }),
        ("user32", "FillRect") => (3, |e, a| {
            let rect = get_rect(e, a[1])?;
            let color = if a[2] < 32 {
                Some(sys_color(a[2].saturating_sub(1)))
            } else {
                e.wm.brush_color(a[2])
            };
            if let Some(c) = color {
                e.wm.fill_rect_dc(a[0], rect, c);
            }
            Ok(1)
        }),
        ("user32", "DrawTextA") => (5, |e, a| draw_text(e, a, false)),
        ("user32", "DrawTextW") => (5, |e, a| draw_text(e, a, true)),
        ("user32", "GetSysColor") => (1, |_, a| Ok(colorref(sys_color(a[0])))),
        ("user32", "GetSysColorBrush") => (1, |e, a| {
            Ok(e.wm.add_gdi(Gdi::Brush {
                color: sys_color(a[0]),
                null: false,
            }))
        }),

        // ---- cursors & icons (no real cursor; handles just have to be non-null) ----
        ("user32", "LoadCursorA") | ("user32", "LoadIconA") => {
            (2, |_, a| Ok(0x5000 + (a[1] & 0xffff)))
        }
        ("user32", "SetCursor") | ("user32", "ShowCursor") => (1, |_, _| Ok(0)),
        ("user32", "GetKeyState") | ("user32", "GetAsyncKeyState") => (1, |e, a| {
            Ok(if e.wm.key_down[(a[0] & 0xff) as usize] {
                0x8000
            } else {
                0
            })
        }),
        ("user32", "MessageBeep") => (1, |_, _| Ok(1)),

        // ---- GDI32 ----
        ("gdi32", "GetStockObject") => (1, |_, a| {
            Ok(match a[0] {
                0 => stock::WHITE_BRUSH,
                1 => stock::LTGRAY_BRUSH,
                2 => stock::GRAY_BRUSH,
                3 => stock::DKGRAY_BRUSH,
                4 => stock::BLACK_BRUSH,
                5 => stock::NULL_BRUSH,
                6 => stock::WHITE_PEN,
                7 => stock::BLACK_PEN,
                8 => stock::NULL_PEN,
                _ => stock::FONT,
            })
        }),
        ("gdi32", "CreateSolidBrush") => (1, |e, a| {
            Ok(e.wm.add_gdi(Gdi::Brush {
                color: colorref(a[0]),
                null: false,
            }))
        }),
        ("gdi32", "CreatePen") => (3, |e, a| {
            Ok(e.wm.add_gdi(Gdi::Pen {
                color: colorref(a[2]),
                width: a[1] as i32,
                null: a[0] == 5,
            }))
        }),
        ("gdi32", "CreateFontA") | ("gdi32", "CreateFontW") => (14, |e, a| {
            let h = a[0] as i32;
            Ok(e.wm.add_gdi(Gdi::Font {
                px: if h == 0 { 13 } else { h.unsigned_abs() },
            }))
        }),
        ("gdi32", "SelectObject") => (2, select_object),
        ("gdi32", "DeleteObject") => (1, |e, a| Ok(e.wm.delete_gdi(a[0]) as u32)),
        ("gdi32", "DeleteDC") => (1, |e, a| Ok(e.wm.gdi.remove(&a[0]).is_some() as u32)),
        ("gdi32", "CreateCompatibleDC") => (1, |e, _| Ok(e.wm.new_dc(Target::Mem))),
        ("gdi32", "CreateCompatibleBitmap") => (3, |e, a| {
            let (w, h) = (a[1].min(4096), a[2].min(4096));
            Ok(e.wm.add_gdi(Gdi::Bitmap(crate::gui::gfx::Framebuffer::new(w, h))))
        }),
        ("gdi32", "SetTextColor") => (2, |e, a| {
            Ok(e.wm.dc_mut(a[0]).map_or(0, |d| {
                colorref(std::mem::replace(&mut d.text_color, colorref(a[1])))
            }))
        }),
        ("gdi32", "SetBkColor") => (2, |e, a| {
            Ok(e.wm.dc_mut(a[0]).map_or(0, |d| {
                colorref(std::mem::replace(&mut d.bk_color, colorref(a[1])))
            }))
        }),
        ("gdi32", "SetBkMode") => (2, |e, a| {
            Ok(e.wm.dc_mut(a[0]).map_or(0, |d| {
                let old = if d.transparent { 1 } else { 2 };
                d.transparent = a[1] == 1;
                old
            }))
        }),
        ("gdi32", "TextOutA") => (5, |e, a| text_out(e, a, false)),
        ("gdi32", "TextOutW") => (5, |e, a| text_out(e, a, true)),
        ("gdi32", "GetTextExtentPoint32A") => (4, |e, a| text_extent(e, a, false)),
        ("gdi32", "GetTextExtentPoint32W") => (4, |e, a| text_extent(e, a, true)),
        ("gdi32", "Rectangle") => (5, |e, a| {
            e.wm.rectangle(a[0], (a[1] as i32, a[2] as i32, a[3] as i32, a[4] as i32));
            Ok(1)
        }),
        ("gdi32", "MoveToEx") => (4, |e, a| {
            let Some(d) = e.wm.dc_mut(a[0]) else {
                return Ok(0);
            };
            let old = std::mem::replace(&mut d.pos, (a[1] as i32, a[2] as i32));
            if a[3] != 0 {
                e.mem.write(a[3], 4, old.0 as u32)?;
                e.mem.write(a[3] + 4, 4, old.1 as u32)?;
            }
            Ok(1)
        }),
        ("gdi32", "LineTo") => (3, |e, a| {
            e.wm.line_to(a[0], a[1] as i32, a[2] as i32);
            Ok(1)
        }),
        ("gdi32", "SetPixel") => (4, |e, a| {
            e.wm.set_pixel(a[0], a[1] as i32, a[2] as i32, colorref(a[3]));
            Ok(a[3])
        }),
        ("gdi32", "GetPixel") => (3, |e, a| {
            Ok(e.wm
                .get_pixel(a[0], a[1] as i32, a[2] as i32)
                .map_or(u32::MAX, colorref))
        }),
        ("gdi32", "BitBlt") => (9, |e, a| {
            Ok(e.wm.bit_blt(
                a[0],
                a[1] as i32,
                a[2] as i32,
                a[3],
                a[4],
                a[5],
                a[6] as i32,
                a[7] as i32,
            ) as u32)
        }),
        ("gdi32", "GetDeviceCaps") => (2, |_, a| {
            Ok(match a[1] {
                8 => DESKTOP_W,
                10 => DESKTOP_H,
                12 => 32,
                14 => 1,
                88 | 90 => 96,
                24 => u32::MAX,
                _ => 0,
            })
        }),
        // ---- Unicode strings ----
        ("user32", "MessageBoxW") => (4, |e, a| {
            let (text, title) = (e.mem.read_wstr(a[1])?, e.mem.read_wstr(a[2])?);
            e.win.messages.push(format!("{title}: {text}"));
            Ok(1)
        }),
        ("user32", "LoadStringW") => (4, |e, a| {
            crate::win32::kernel32_extra::load_string(e, a, true)
        }),
        ("kernel32", "lstrlenW") => (1, |e, a| {
            Ok(e.mem.read_wstr(a[0])?.encode_utf16().count() as u32)
        }),
        ("kernel32", "GetCommandLineW") => (0, |e, _| {
            if let Some(&p) = e.win.data_syms.get("__cmdline_w") {
                return Ok(p);
            }
            let p = e.heap_alloc(64)?;
            e.mem.write_wstr(p, "game.exe")?;
            e.win.data_syms.insert("__cmdline_w".into(), p);
            Ok(p)
        }),
        ("advapi32", "RegCreateKeyExW") => (9, |_, _| Ok(5)),
        ("advapi32", "RegSetValueExW") => (6, |_, _| Ok(5)),
        ("advapi32", "RegQueryValueExW") => (6, |_, _| Ok(2)),

        // ---- rectangles & misc USER32 ----
        ("user32", "CopyRect") => (2, |e, a| {
            let r = get_rect(e, a[1])?;
            put_rect(e, a[0], r)?;
            Ok(1)
        }),
        ("user32", "IntersectRect") => (3, |e, a| {
            let (p, q) = (get_rect(e, a[1])?, get_rect(e, a[2])?);
            let r = (p.0.max(q.0), p.1.max(q.1), p.2.min(q.2), p.3.min(q.3));
            let ok = r.0 < r.2 && r.1 < r.3;
            put_rect(e, a[0], if ok { r } else { (0, 0, 0, 0) })?;
            Ok(ok as u32)
        }),
        ("user32", "PtInRect") => (3, |e, a| {
            let (l, t, r, b) = get_rect(e, a[0])?;
            let (x, y) = (a[1] as i32, a[2] as i32);
            Ok((x >= l && x < r && y >= t && y < b) as u32)
        }),
        ("user32", "InflateRect") => (3, |e, a| {
            let (l, t, r, b) = get_rect(e, a[0])?;
            let (dx, dy) = (a[1] as i32, a[2] as i32);
            put_rect(e, a[0], (l - dx, t - dy, r + dx, b + dy))?;
            Ok(1)
        }),
        ("user32", "AdjustWindowRect") => (3, |e, a| {
            let (l, t, r, b) = get_rect(e, a[0])?;
            let (bw, cap) = if a[1] & WS_CAPTION == WS_CAPTION {
                (3, 19)
            } else if a[1] & WS_BORDER != 0 {
                (1, 0)
            } else {
                (0, 0)
            };
            let cap = cap + if a[2] != 0 { MENU_H } else { 0 };
            put_rect(e, a[0], (l - bw, t - bw - cap, r + bw, b + bw))?;
            Ok(1)
        }),
        ("user32", "FrameRect") => (3, |e, a| {
            let (l, t, r, b) = get_rect(e, a[1])?;
            if let Some(c) = e.wm.brush_color(a[2]) {
                e.wm.fill_rect_dc(a[0], (l, t, r, t + 1), c);
                e.wm.fill_rect_dc(a[0], (l, b - 1, r, b), c);
                e.wm.fill_rect_dc(a[0], (l, t, l + 1, b), c);
                e.wm.fill_rect_dc(a[0], (r - 1, t, r, b), c);
            }
            Ok(1)
        }),
        ("user32", "InvertRect") => (2, |e, a| {
            let r = get_rect(e, a[1])?;
            e.wm.invert_rect(a[0], r);
            Ok(1)
        }),
        ("user32", "GetCapture") => (0, |e, _| Ok(e.wm.capture)),
        ("user32", "IsIconic") => (1, |_, _| Ok(0)),
        ("user32", "LoadImageW") | ("user32", "LoadImageA") => {
            (6, |_, a| Ok(0x6000 + (a[1] & 0xffff)))
        }
        ("user32", "LoadAcceleratorsA") => (2, |e, a| load_accelerators(e, a, false)),
        ("user32", "LoadAcceleratorsW") => (2, |e, a| load_accelerators(e, a, true)),
        ("user32", "TranslateAcceleratorA") | ("user32", "TranslateAcceleratorW") => {
            (3, translate_accelerator)
        }

        // ---- menus ----
        ("user32", "LoadMenuA") => (2, |e, a| load_menu(e, a, false)),
        ("user32", "LoadMenuW") => (2, |e, a| load_menu(e, a, true)),
        ("user32", "SetMenu") => (2, |e, a| {
            e.wm.set_menu(a[0], a[1]);
            Ok(1)
        }),
        ("user32", "GetMenu") => (1, |e, a| Ok(e.wm.window(a[0]).map_or(0, |w| w.menu))),
        ("user32", "GetSubMenu") => (2, |e, a| {
            Ok(e.wm
                .menus
                .get(&a[0])
                .and_then(|m| m.items.get(a[1] as usize))
                .map_or(0, |i| i.submenu))
        }),
        ("user32", "GetMenuItemCount") => (1, |e, a| {
            Ok(e.wm
                .menus
                .get(&a[0])
                .map_or(u32::MAX, |m| m.items.len() as u32))
        }),
        ("user32", "CheckMenuItem") => (3, |e, a| menu_flag(e, a, MF_CHECKED, MF_CHECKED)),
        ("user32", "EnableMenuItem") => (3, |e, a| menu_flag(e, a, MF_GRAYED | 2, MF_GRAYED)),
        ("user32", "CheckMenuRadioItem") => (5, |e, a| {
            let (menu, first, last, check, by_pos) = (a[0], a[1], a[2], a[3], a[4] & 0x400 != 0);
            for id in first..=last {
                if let Some((m, i)) = e.wm.find_item(menu, id, by_pos) {
                    let item = &mut e.wm.menus.get_mut(&m).unwrap().items[i];
                    if id == check {
                        item.flags |= MF_CHECKED;
                    } else {
                        item.flags &= !MF_CHECKED;
                    }
                }
            }
            Ok(1)
        }),
        ("user32", "DrawMenuBar") => (1, |_, _| Ok(1)),
        ("user32", "DestroyMenu") => (1, |e, a| Ok(e.wm.menus.remove(&a[0]).is_some() as u32)),
        ("user32", "CreateMenu") | ("user32", "CreatePopupMenu") => (0, |e, _| Ok(e.wm.new_menu())),
        ("user32", "AppendMenuA") => (4, |e, a| append_menu(e, a, false)),
        ("user32", "AppendMenuW") => (4, |e, a| append_menu(e, a, true)),
        ("user32", "RegisterWindowMessageW") | ("user32", "RegisterWindowMessageA") => {
            (1, |_, _| Ok(0xC100))
        }
        ("user32", "SetCursorPos") | ("user32", "WinHelpW") => (2, |_, _| Ok(1)),
        // Real animation loops use this as a sleep; reporting a timeout keeps them moving.
        ("user32", "MsgWaitForMultipleObjects") => (5, |_, _| Ok(258)),
        // ---- dialogs ----
        ("user32", "DialogBoxParamA") => (5, |e, a| dialog_box(e, a, false)),
        ("user32", "DialogBoxParamW") => (5, |e, a| dialog_box(e, a, true)),
        ("user32", "CreateDialogParamA") => (5, |e, a| create_dialog(e, a, false)),
        ("user32", "CreateDialogParamW") => (5, |e, a| create_dialog(e, a, true)),
        ("user32", "EndDialog") => (2, |e, a| {
            Ok(match e.wm.modal.as_mut().filter(|m| m.hwnd == a[0]) {
                Some(m) => {
                    m.result = Some(a[1] as i32);
                    1
                }
                None => 0,
            })
        }),
        ("user32", "CheckDlgButton") => (3, |e, a| {
            let Some(h) = dlg_item(e, a[0], a[1]) else {
                return Ok(0);
            };
            if let Some(w) = e.wm.window_mut(h) {
                w.check = a[2];
            }
            Ok(1)
        }),
        ("user32", "IsDlgButtonChecked") => (2, |e, a| {
            Ok(dlg_item(e, a[0], a[1])
                .and_then(|h| e.wm.window(h))
                .map_or(0, |w| w.check))
        }),
        ("user32", "CheckRadioButton") => (4, |e, a| {
            for id in a[1]..=a[2] {
                if let Some(h) = dlg_item(e, a[0], id)
                    && let Some(w) = e.wm.window_mut(h)
                {
                    w.check = (id == a[3]) as u32;
                }
            }
            Ok(1)
        }),
        ("user32", "SendDlgItemMessageA") | ("user32", "SendDlgItemMessageW") => {
            (5, |e, a| match dlg_item(e, a[0], a[1]) {
                Some(h) => send_message(e, h, a[2], a[3], a[4]),
                None => Ok(0),
            })
        }
        ("user32", "SetDlgItemTextA") => (3, |e, a| set_dlg_item_text(e, a, false)),
        ("user32", "SetDlgItemTextW") => (3, |e, a| set_dlg_item_text(e, a, true)),
        ("user32", "GetDlgCtrlID") => (1, |e, a| Ok(e.wm.window(a[0]).map_or(0, |w| w.id))),
        ("user32", "IsDialogMessageA") | ("user32", "IsDialogMessageW") => (2, |_, _| Ok(0)),
        ("user32", "SetWindowTextW") => (2, |e, a| {
            let t = e.mem.read_wstr(a[1])?;
            Ok(e.wm.window_mut(a[0]).map(|w| w.title = t).is_some() as u32)
        }),
        ("shell32", "ShellAboutW") | ("shell32", "ShellAboutA") => (4, |_, _| Ok(1)),
        ("comctl32", "InitCommonControlsEx") => (1, |_, _| Ok(1)),

        ("user32", "LoadBitmapA") => (2, |e, a| load_bitmap(e, a, false)),
        ("user32", "LoadBitmapW") => (2, |e, a| load_bitmap(e, a, true)),

        // ---- more GDI32 ----
        ("gdi32", "GetDCOrgEx") => (2, |e, a| {
            e.mem.write(a[1], 4, 0)?;
            e.mem.write(a[1] + 4, 4, 0)?;
            Ok(1)
        }),
        ("gdi32", "GetObjectA") | ("gdi32", "GetObjectW") => (3, get_object),
        ("gdi32", "StretchBlt") => (11, |e, a| {
            let i = |n: usize| a[n] as i32;
            Ok(e.wm
                .stretch_blt(a[0], i(1), i(2), i(3), i(4), a[5], i(6), i(7), i(8), i(9))
                as u32)
        }),
        ("gdi32", "PtVisible") => (3, |_, _| Ok(1)),
        ("gdi32", "SetBrushOrgEx") => (4, |_, _| Ok(1)),
        ("gdi32", "SetROP2") => (2, |_, _| Ok(13)), // R2_COPYPEN
        ("gdi32", "PatBlt") => (6, |e, a| {
            let rect = (
                a[1] as i32,
                a[2] as i32,
                a[1] as i32 + a[3] as i32,
                a[2] as i32 + a[4] as i32,
            );
            match a[5] {
                0x00F0_0021 => e.wm.brush_rect(a[0], rect),  // PATCOPY
                0x0055_0009 => e.wm.invert_rect(a[0], rect), // DSTINVERT
                0x0000_0042 => e.wm.fill_rect_dc(a[0], rect, 0), // BLACKNESS
                0x00FF_0062 => e.wm.fill_rect_dc(a[0], rect, 0xFFFFFF), // WHITENESS
                _ => {}
            }
            Ok(1)
        }),
        ("gdi32", "LineDDA") => (6, line_dda),
        ("gdi32", "GetTextMetricsW") | ("gdi32", "GetTextMetricsA") => (2, |e, a| {
            let (_, h) = e.wm.measure_text(a[0], "M");
            let (avg, _) = e.wm.measure_text(a[0], "x");
            let ascent = h * 4 / 5;
            for (i, v) in [h, ascent, h - ascent, 0, 0, avg, avg * 2, 400]
                .iter()
                .enumerate()
            {
                e.mem.write(a[1] + 4 * i as u32, 4, *v)?;
            }
            Ok(1)
        }),
        _ => return None,
    };
    Some(r)
}

// ---- helpers -------------------------------------------------------------------------------

fn get_rect(e: &Emu, addr: u32) -> Res<(i32, i32, i32, i32)> {
    let r = |o| e.mem.read(addr + o, 4).map(|v| v as i32);
    Ok((r(0)?, r(4)?, r(8)?, r(12)?))
}

fn put_rect(e: &mut Emu, addr: u32, r: (i32, i32, i32, i32)) -> Res<()> {
    for (i, v) in [r.0, r.1, r.2, r.3].iter().enumerate() {
        e.mem.write(addr + 4 * i as u32, 4, *v as u32)?;
    }
    Ok(())
}

/// Convert the POINT at `p` between client and screen space (`dir` = 1 or -1).
fn shift_point(e: &mut Emu, hwnd: u32, p: u32, dir: i32) -> Res<u32> {
    let Some((ox, oy)) = e.wm.client_origin(hwnd) else {
        return Ok(0);
    };
    let (x, y) = (e.mem.read(p, 4)? as i32, e.mem.read(p + 4, 4)? as i32);
    e.mem.write(p, 4, (x + dir * ox) as u32)?;
    e.mem.write(p + 4, 4, (y + dir * oy) as u32)?;
    Ok(1)
}

/// Copy a title into a guest buffer like GetWindowText: truncated, always NUL-terminated.
fn copy_text(e: &mut Emu, buf: u32, max: u32, text: &str) -> Res<u32> {
    if max == 0 {
        return Ok(0);
    }
    let bytes = &text.as_bytes()[..text.len().min(max as usize - 1)];
    e.mem.write_bytes(buf, bytes)?;
    e.mem.write(buf + bytes.len() as u32, 1, 0)?;
    Ok(bytes.len() as u32)
}

fn get_window_long(e: &Emu, hwnd: u32, index: i32) -> u32 {
    let Some(w) = e.wm.window(hwnd) else { return 0 };
    match index {
        -4 => w.wndproc,
        -6 => e.win.image_base,
        -12 => w.id,
        -16 => w.style,
        -20 => w.ex_style,
        -21 => w.userdata,
        _ => 0,
    }
}

/// Class name argument: a string pointer, or an atom from RegisterClass.
fn class_name(e: &Emu, v: u32, wide: bool) -> Res<String> {
    if v < 0x1_0000 {
        let idx = v.wrapping_sub(0xC000) as usize;
        return Ok(e.wm.class_atoms.get(idx).cloned().unwrap_or_default());
    }
    read_str(e, v, wide)
}

fn read_str(e: &Emu, p: u32, wide: bool) -> Res<String> {
    Ok(if wide {
        e.mem.read_wstr(p)?
    } else {
        e.mem.read_cstr(p)?
    })
}

fn register_class(e: &mut Emu, p: u32, ex: bool, wide: bool) -> Res<u32> {
    let o = if ex { 4 } else { 0 };
    let wndproc = e.mem.read(p + o + 4, 4)?;
    let brush = e.mem.read(p + o + 28, 4)?;
    let name = read_str(e, e.mem.read(p + o + 36, 4)?, wide)?;
    let bg = if brush == 0 {
        0xFFFFFF
    } else if brush < 32 {
        sys_color(brush - 1)
    } else {
        e.wm.brush_color(brush).unwrap_or(0xFFFFFF)
    };
    let key = name.to_ascii_lowercase();
    if e.wm.classes.contains_key(&key) {
        e.win.last_error = 1410; // ERROR_CLASS_ALREADY_EXISTS
        return Ok(0);
    }
    let menu_name = e.mem.read(p + o + 32, 4)?;
    let style = e.mem.read(p + o, 4)?;
    e.wm.classes.insert(
        key,
        WndClass {
            wndproc,
            bg,
            menu_name,
            wide,
            style,
        },
    );
    e.wm.class_atoms.push(name);
    Ok(0xC000 + e.wm.class_atoms.len() as u32 - 1)
}

const RT_MENU: u32 = 4;
const RT_ACCELERATOR: u32 = 9;

fn create_window(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let class = class_name(e, a[1], wide)?;
    if !e.wm.classes.contains_key(&class.to_ascii_lowercase())
        && !WindowManager::is_builtin_class(&class)
    {
        e.win.last_error = 1407; // ERROR_CANNOT_FIND_WND_CLASS
        return Ok(0);
    }
    let title = if a[2] != 0 {
        read_str(e, a[2], wide)?
    } else {
        String::new()
    };
    let (style, parent) = (a[3], a[8]);
    let id = if style & WS_CHILD != 0 { a[9] } else { 0 };
    let hwnd = e.wm.add_window(
        &class,
        &title,
        style,
        a[0],
        (a[4], a[5], a[6], a[7]),
        parent,
        id,
    );
    if style & WS_CHILD == 0 {
        let mut menu = if e.wm.menus.contains_key(&a[9]) {
            a[9]
        } else {
            0
        };
        if menu == 0
            && let Some((name, wide)) =
                e.wm.classes
                    .get(&class.to_ascii_lowercase())
                    .map(|c| (c.menu_name, c.wide))
                    .filter(|c| c.0 != 0)
            && let Some(bytes) =
                crate::win32::kernel32_extra::resource_bytes(e, a[10], RT_MENU, name, wide)?
        {
            menu = e.wm.load_menu_template(&bytes).unwrap_or(0);
        }
        e.wm.set_menu(hwnd, menu);
    }

    // CREATESTRUCTA
    let cs = e.heap_alloc(48)?;
    for (off, v) in [
        (0, a[11]),
        (4, a[10]),
        (8, a[9]),
        (12, parent),
        (16, a[7]),
        (20, a[6]),
        (24, a[5]),
        (28, a[4]),
        (32, style),
        (36, a[2]),
        (40, a[1]),
        (44, a[0]),
    ] {
        e.mem.write(cs + off, 4, v)?;
    }
    if send_message(e, hwnd, WM_NCCREATE, 0, cs)? == 0
        || send_message(e, hwnd, WM_CREATE, 0, cs)? as i32 == -1
    {
        e.wm.remove_window(hwnd);
        return Ok(0);
    }
    // WM_SIZE/WM_MOVE are not sent here: apps like Solitaire have not built their state
    // yet. They go out the first time the window is shown (see show_window).
    if style & WS_VISIBLE != 0 {
        show_window(e, hwnd, true)?;
        activate_top_level(e, hwnd)?;
    }
    Ok(hwnd)
}

/// A top-level window that appears becomes the active one, as on Windows.
fn activate_top_level(e: &mut Emu, hwnd: u32) -> Res<()> {
    if e.wm.window(hwnd).is_some_and(|w| w.parent == 0 && w.style & WS_CHILD == 0) {
        activate(e, hwnd)?;
    }
    Ok(())
}

/// Deliver a message synchronously, to guest code or to the built-in handler.
pub fn send_message(e: &mut Emu, hwnd: u32, msg: u32, w: u32, l: u32) -> Res<u32> {
    let Some((proc, is_dialog)) = e.wm.window(hwnd).map(|win| (win.wndproc, win.is_dialog)) else {
        return Ok(0);
    };
    if proc == 0 {
        return default_proc(e, hwnd, msg, w, l);
    }
    let r = e.call_function(proc, &[hwnd, msg, w, l])?;
    // A DLGPROC reports "handled" with TRUE; otherwise the system's dialog handling runs.
    if is_dialog && r == 0 && msg != WM_INITDIALOG {
        return default_proc(e, hwnd, msg, w, l);
    }
    Ok(r)
}

fn default_proc(e: &mut Emu, hwnd: u32, msg: u32, w: u32, l: u32) -> Res<u32> {
    match msg {
        WM_NCCREATE => Ok(1),
        WM_CLOSE if e.wm.window(hwnd).is_some_and(|w| w.is_dialog) => {
            const IDCANCEL: u32 = 2;
            send_message(e, hwnd, WM_COMMAND, IDCANCEL, 0)
        }
        WM_CLOSE => {
            destroy_window(e, hwnd)?;
            Ok(0)
        }
        BM_GETCHECK => Ok(e.wm.window(hwnd).map_or(0, |win| win.check)),
        BM_SETCHECK => {
            if let Some(win) = e.wm.window_mut(hwnd) {
                win.check = w;
            }
            Ok(0)
        }
        WM_ERASEBKGND => {
            let Some(bg) = e.wm.window(hwnd).map(|win| win.bg) else {
                return Ok(0);
            };
            if let Some((cw, ch)) = e.wm.client_size(hwnd) {
                e.wm.fill_rect_dc(w, (0, 0, cw, ch), bg);
            }
            Ok(1)
        }
        WM_PAINT => {
            if let Some(win) = e.wm.window_mut(hwnd) {
                win.needs_paint = false;
            }
            Ok(0)
        }
        WM_SETTEXT => {
            let t = e.mem.read_cstr(l)?;
            if let Some(win) = e.wm.window_mut(hwnd) {
                win.title = t;
            }
            Ok(1)
        }
        WM_GETTEXT => {
            let t =
                e.wm.window(hwnd)
                    .map(|win| win.title.clone())
                    .unwrap_or_default();
            copy_text(e, l, w, &t)
        }
        WM_GETTEXTLENGTH => Ok(e.wm.window(hwnd).map_or(0, |win| win.title.len() as u32)),
        WM_NCHITTEST => Ok(1), // HTCLIENT
        _ => Ok(0),
    }
}

/// DestroyWindow: WM_DESTROY goes to the window and then each descendant, before removal.
pub fn destroy_window(e: &mut Emu, hwnd: u32) -> Res<()> {
    let mut order = vec![hwnd];
    let mut i = 0;
    while i < order.len() {
        order.extend(e.wm.children_of(order[i]));
        i += 1;
    }
    for h in order {
        send_message(e, h, WM_DESTROY, 0, 0)?;
    }
    e.wm.remove_window(hwnd);
    Ok(())
}

/// Make `hwnd`'s top-level window the active one, telling it so at once, as Windows does
/// (WM_ACTIVATEAPP, WM_ACTIVATE, WM_SETFOCUS are sent, not posted).
fn activate(e: &mut Emu, hwnd: u32) -> Res<()> {
    if e.wm.window(hwnd).is_none() || e.wm.active == hwnd {
        return Ok(());
    }
    let first = e.wm.active == 0;
    e.wm.active = hwnd;
    if first {
        send_message(e, hwnd, 0x1C, 1, 0)?; // WM_ACTIVATEAPP: the application is now active
    }
    send_message(e, hwnd, 6, 1, 0)?; // WM_ACTIVATE, WA_ACTIVE
    if e.wm.focus == 0 {
        e.wm.focus = hwnd;
        send_message(e, hwnd, 7, 0, 0)?; // WM_SETFOCUS
    }
    Ok(())
}

fn show_window(e: &mut Emu, hwnd: u32, show: bool) -> Res<u32> {
    let Some(win) = e.wm.window_mut(hwnd) else {
        return Ok(0);
    };
    let was = win.visible;
    win.visible = show;
    win.needs_paint |= show;
    let first_geometry = show && !std::mem::replace(&mut win.geometry_sent, true);
    if was != show {
        send_message(e, hwnd, WM_SHOWWINDOW, show as u32, 0)?;
    }
    if first_geometry && let (Some((cw, ch)), Some(w)) = (e.wm.client_size(hwnd), e.wm.window(hwnd))
    {
        let (x, y) = (w.x, w.y);
        send_message(
            e,
            hwnd,
            WM_SIZE,
            0,
            ((ch as u32) << 16) | (cw as u32 & 0xffff),
        )?;
        send_message(
            e,
            hwnd,
            WM_MOVE,
            0,
            ((y as u32 & 0xffff) << 16) | (x as u32 & 0xffff),
        )?;
    }
    Ok(was as u32)
}

pub(crate) fn move_window(e: &mut Emu, hwnd: u32, x: i32, y: i32, w: i32, h: i32) -> Res<()> {
    let Some(win) = e.wm.window(hwnd) else {
        return Ok(());
    };
    let (moved, resized) = ((win.x, win.y) != (x, y), (win.w, win.h) != (w, h));
    e.wm.set_geometry(hwnd, x, y, w, h);
    if resized && let Some((cw, ch)) = e.wm.client_size(hwnd) {
        send_message(
            e,
            hwnd,
            WM_SIZE,
            0,
            ((ch as u32) << 16) | cw as u32 & 0xffff,
        )?;
    }
    if moved {
        send_message(
            e,
            hwnd,
            WM_MOVE,
            0,
            ((y as u32 & 0xffff) << 16) | x as u32 & 0xffff,
        )?;
    }
    Ok(())
}

fn set_window_pos(e: &mut Emu, a: &Args) -> Res<u32> {
    let (hwnd, flags) = (a[0], a[6]);
    let Some(win) = e.wm.window(hwnd) else {
        return Ok(0);
    };
    let (x, y) = if flags & 2 != 0 {
        (win.x, win.y)
    } else {
        (a[2] as i32, a[3] as i32)
    };
    let (w, h) = if flags & 1 != 0 {
        (win.w, win.h)
    } else {
        (a[4] as i32, a[5] as i32)
    };
    move_window(e, hwnd, x, y, w, h)?;
    if flags & 0x40 != 0 {
        show_window(e, hwnd, true)?;
    }
    if flags & 0x80 != 0 {
        show_window(e, hwnd, false)?;
    }
    Ok(1)
}

fn write_msg(e: &mut Emu, p: u32, m: &Msg) -> Res<()> {
    for (i, v) in [m.hwnd, m.message, m.wparam, m.lparam, m.time, 0, 0]
        .iter()
        .enumerate()
    {
        e.mem.write(p + 4 * i as u32, 4, *v)?;
    }
    Ok(())
}

fn get_message(e: &mut Emu, a: &Args) -> Res<u32> {
    e.wm.now_ms = e.wm.now_ms.max(e.win.elapsed_ms());
    match e.wm.next_message(true) {
        Some(m) => {
            write_msg(e, a[0], &m)?;
            Ok((m.message != WM_QUIT) as u32)
        }
        // Nothing to deliver: the host must supply input (or time) and re-run this call.
        None => Err(Stop::Blocked(crate::win32::threads::Block::Input)),
    }
}

fn peek_message(e: &mut Emu, a: &Args) -> Res<u32> {
    e.wm.now_ms = e.wm.now_ms.max(e.win.elapsed_ms());
    match e.wm.next_message(a[4] & 1 != 0) {
        Some(m) => {
            write_msg(e, a[0], &m)?;
            Ok(1)
        }
        None => Ok(0),
    }
}

fn dispatch_message(e: &mut Emu, a: &Args) -> Res<u32> {
    let rd = |o| e.mem.read(a[0] + o, 4);
    let (hwnd, msg, w, l) = (rd(0)?, rd(4)?, rd(8)?, rd(12)?);
    if msg == WM_TIMER && l != 0 {
        let now = e.wm.now_ms as u32;
        return e.call_function(l, &[hwnd, WM_TIMER, w, now]);
    }
    if hwnd == 0 {
        return Ok(0);
    }
    let r = send_message(e, hwnd, msg, w, l)?;
    if msg == WM_PAINT {
        // A WndProc that never validates would otherwise be repainted forever.
        if let Some(win) = e.wm.window_mut(hwnd) {
            win.needs_paint = false;
        }
    }
    Ok(r)
}

fn begin_paint(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some((cw, ch)) = e.wm.client_size(a[0]) else {
        return Ok(0);
    };
    if let Some(w) = e.wm.window_mut(a[0]) {
        w.needs_paint = false;
    }
    let hdc = e.wm.new_dc(Target::Window(a[0]));
    e.mem.write_bytes(a[1], &[0; 64])?;
    e.mem.write(a[1], 4, hdc)?;
    e.mem.write(a[1] + 4, 4, 1)?; // fErase
    put_rect(e, a[1] + 8, (0, 0, cw, ch))?;
    Ok(hdc)
}

fn select_object(e: &mut Emu, a: &Args) -> Res<u32> {
    let kind = match e.wm.gdi.get(&a[1]) {
        Some(Gdi::Pen { .. }) => 0,
        Some(Gdi::Brush { .. }) => 1,
        Some(Gdi::Font { .. }) => 2,
        Some(Gdi::Bitmap(_)) => 3,
        _ => return Ok(0),
    };
    let Some(dc) = e.wm.dc_mut(a[0]) else {
        return Ok(0);
    };
    let slot = match kind {
        0 => &mut dc.pen,
        1 => &mut dc.brush,
        2 => &mut dc.font,
        _ => &mut dc.bitmap,
    };
    Ok(std::mem::replace(slot, a[1]))
}

fn draw_text(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let (hdc, text_ptr, count, rect_ptr, flags) = (a[0], a[1], a[2] as i32, a[3], a[4]);
    let text = text_arg(e, text_ptr, count, wide)?;
    let (l, t, r, b) = get_rect(e, rect_ptr)?;
    let (tw, th) = e.wm.measure_text(hdc, &text);
    const DT_CENTER: u32 = 1;
    const DT_RIGHT: u32 = 2;
    const DT_VCENTER: u32 = 4;
    const DT_CALCRECT: u32 = 0x400;
    if flags & DT_CALCRECT != 0 {
        put_rect(e, rect_ptr, (l, t, l + tw as i32, t + th as i32))?;
        return Ok(th);
    }
    let x = if flags & DT_CENTER != 0 {
        l + (r - l - tw as i32) / 2
    } else if flags & DT_RIGHT != 0 {
        r - tw as i32
    } else {
        l
    };
    let y = if flags & DT_VCENTER != 0 {
        t + (b - t - th as i32) / 2
    } else {
        t
    };
    e.wm.text_out(hdc, x, y, &text);
    Ok(th)
}

fn text_arg(e: &Emu, ptr: u32, count: i32, wide: bool) -> Res<String> {
    if count < 0 {
        return read_str(e, ptr, wide);
    }
    Ok(if wide {
        let units: Vec<u16> = (0..count as u32)
            .map(|i| e.mem.read(ptr + 2 * i, 2).map(|c| c as u16))
            .collect::<Result<_, _>>()?;
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&e.mem.read_bytes(ptr, count as u32)?).into_owned()
    })
}

fn text_out(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let text = text_arg(e, a[3], a[4] as i32, wide)?;
    e.wm.text_out(a[0], a[1] as i32, a[2] as i32, &text);
    Ok(1)
}

fn text_extent(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let text = text_arg(e, a[1], a[2] as i32, wide)?;
    let (w, h) = e.wm.measure_text(a[0], &text);
    e.mem.write(a[3], 4, w)?;
    e.mem.write(a[3] + 4, 4, h)?;
    Ok(1)
}

/// LineDDA: call the guest's callback(x, y, lparam) for each pixel of the line.
fn line_dda(e: &mut Emu, a: &Args) -> Res<u32> {
    let (x0, y0, x1, y1) = (a[0] as i32, a[1] as i32, a[2] as i32, a[3] as i32);
    let (cb, lparam) = (a[4], a[5]);
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let (mut x, mut y, mut err) = (x0, y0, dx + dy);
    loop {
        e.call_function(cb, &[x as u32, y as u32, lparam])?;
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
    Ok(1)
}

fn load_bitmap(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    const RT_BITMAP: u32 = 2;
    let Some(dib) = crate::win32::kernel32_extra::resource_bytes(e, a[0], RT_BITMAP, a[1], wide)?
    else {
        e.win.last_error = 1814; // ERROR_RESOURCE_NAME_NOT_FOUND
        return Ok(0);
    };
    Ok(match crate::gui::gfx::Framebuffer::from_dib(&dib) {
        Some(fb) => e.wm.add_gdi(Gdi::Bitmap(fb)),
        None => {
            if e.win.trace {
                eprintln!(
                    "[api] LoadBitmap: undecodable DIB of {} bytes, header {:02x?}",
                    dib.len(),
                    &dib[..dib.len().min(40)]
                );
            }
            0
        }
    })
}

/// GetObject for bitmaps (BITMAP) and brushes (LOGBRUSH).
fn get_object(e: &mut Emu, a: &Args) -> Res<u32> {
    let (h, cb, buf) = (a[0], a[1], a[2]);
    let words: Vec<u32> = match e.wm.gdi.get(&h) {
        // bmType, bmWidth, bmHeight, bmWidthBytes, (bmPlanes | bmBitsPixel << 16), bmBits
        Some(Gdi::Bitmap(fb)) => vec![0, fb.width, fb.height, fb.width * 4, 1 | (32 << 16), 0],
        // lbStyle (BS_SOLID), lbColor, lbHatch
        Some(Gdi::Brush { color, .. }) => vec![0, colorref(*color), 0],
        _ => return Ok(0),
    };
    let n = (words.len() as u32 * 4).min(cb);
    if buf != 0 {
        for (i, w) in words.iter().enumerate().take((n / 4) as usize) {
            e.mem.write(buf + 4 * i as u32, 4, *w)?;
        }
    }
    Ok(n)
}

fn load_menu(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let Some(bytes) = crate::win32::kernel32_extra::resource_bytes(e, a[0], RT_MENU, a[1], wide)?
    else {
        return Ok(0);
    };
    Ok(e.wm.load_menu_template(&bytes).unwrap_or(0))
}

/// CheckMenuItem / EnableMenuItem: `set_mask` bits in `flags` turn the state on, and
/// `state` is the bit stored on the item. Returns the previous state, or -1.
fn menu_flag(e: &mut Emu, a: &Args, set_mask: u32, state: u32) -> Res<u32> {
    let (menu, id, flags) = (a[0], a[1], a[2]);
    let Some((m, i)) = e.wm.find_item(menu, id, flags & 0x400 != 0) else {
        return Ok(u32::MAX);
    };
    let item = &mut e.wm.menus.get_mut(&m).unwrap().items[i];
    let prev = item.flags & state;
    if flags & set_mask != 0 {
        item.flags |= state;
    } else {
        item.flags &= !state;
    }
    Ok(prev)
}

fn append_menu(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let (menu, flags, id, text_ptr) = (a[0], a[1], a[2], a[3]);
    if !e.wm.menus.contains_key(&menu) {
        return Ok(0);
    }
    let sep = flags & MF_SEPARATOR != 0;
    let text = if sep {
        String::new()
    } else {
        read_str(e, text_ptr, wide)?
    };
    let popup = flags & MF_POPUP != 0;
    e.wm.menus.get_mut(&menu).unwrap().items.push(MenuItem {
        text,
        id: if popup { 0 } else { id },
        flags: flags & (MF_GRAYED | MF_CHECKED | MF_SEPARATOR),
        submenu: if popup { id } else { 0 },
    });
    Ok(1)
}

fn load_accelerators(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let Some(bytes) =
        crate::win32::kernel32_extra::resource_bytes(e, a[0], RT_ACCELERATOR, a[1], wide)?
    else {
        return Ok(0);
    };
    let mut table = Vec::new();
    for chunk in bytes.chunks_exact(8) {
        table.push(Accel {
            flags: chunk[0] & 0x7f,
            key: u16::from_le_bytes([chunk[2], chunk[3]]),
            id: u16::from_le_bytes([chunk[4], chunk[5]]),
        });
        if chunk[0] & 0x80 != 0 {
            break;
        }
    }
    let handle = 0x7000_0000 + e.wm.accels.len() as u32 * 4 + 4;
    e.wm.accels.insert(handle, table);
    Ok(handle)
}

/// If the key message matches an accelerator, deliver WM_COMMAND and report it handled.
fn translate_accelerator(e: &mut Emu, a: &Args) -> Res<u32> {
    let (hwnd, haccel, msg_ptr) = (a[0], a[1], a[2]);
    let (message, vk) = (e.mem.read(msg_ptr + 4, 4)?, e.mem.read(msg_ptr + 8, 4)?);
    if message != WM_KEYDOWN && message != WM_SYSKEYDOWN {
        return Ok(0);
    }
    let Some(id) =
        e.wm.accels
            .get(&haccel)
            .and_then(|t| t.iter().find(|acc| e.wm.accel_matches(acc, vk)))
            .map(|acc| acc.id)
    else {
        return Ok(0);
    };
    send_message(e, hwnd, WM_COMMAND, id as u32 | (1 << 16), 0)?;
    Ok(1)
}

const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const RT_DIALOG: u32 = 5;

/// The control with command `id` inside dialog `dlg`.
fn dlg_item(e: &Emu, dlg: u32, id: u32) -> Option<u32> {
    e.wm.windows
        .iter()
        .find(|w| w.parent == dlg && w.id == id)
        .map(|w| w.hwnd)
}

/// Build the dialog window and its controls from a template and send WM_INITDIALOG.
fn build_dialog(e: &mut Emu, t: &DlgTemplate, parent: u32, dlgproc: u32, lparam: u32) -> Res<u32> {
    const WS_CAPTION_BITS: u32 = 0x00C0_0000;
    let (cw, ch) = (dlu_x(t.cx), dlu_y(t.cy));
    let style = if t.title.is_empty() {
        t.style
    } else {
        t.style | WS_CAPTION_BITS
    };
    let (bw, cap) = if style & WS_CAPTION_BITS == WS_CAPTION_BITS {
        (3, 19)
    } else if style & WS_BORDER != 0 {
        (1, 0)
    } else {
        (0, 0)
    };
    let (w, h) = (cw + 2 * bw, ch + cap + 2 * bw);
    // Centred over the owner (or the desktop).
    let (px, py, pw, ph) = match e.wm.window(parent) {
        Some(o) => (o.x, o.y, o.w, o.h),
        None => (0, 0, DESKTOP_W as i32, DESKTOP_H as i32),
    };
    let (x, y) = (px + (pw - w) / 2, py + (ph - h) / 2);
    let hwnd = e.wm.add_window(
        "#32770",
        &t.title,
        style,
        t.ex_style,
        (x as u32, y as u32, w as u32, h as u32),
        0,
        0,
    );
    if let Some(win) = e.wm.window_mut(hwnd) {
        win.wndproc = dlgproc;
        win.is_dialog = true;
    }
    let mut first_tab = 0;
    for it in &t.items {
        let child = e.wm.add_window(
            &it.class,
            &it.title,
            it.style | WS_CHILD,
            it.ex_style,
            (
                dlu_x(it.x) as u32,
                dlu_y(it.y) as u32,
                dlu_x(it.cx) as u32,
                dlu_y(it.cy) as u32,
            ),
            hwnd,
            it.id,
        );
        if let Some(win) = e.wm.window_mut(child) {
            win.visible = it.style & WS_VISIBLE != 0;
        }
        if first_tab == 0 && it.style & 0x0001_0000 != 0 {
            first_tab = child; // WS_TABSTOP
        }
    }
    let wants_focus = send_message(e, hwnd, WM_INITDIALOG, first_tab, lparam)?;
    let _ = wants_focus;
    if let Some(win) = e.wm.window_mut(hwnd) {
        win.visible = true;
        win.needs_paint = true;
    }
    for child in e.wm.children_of(hwnd) {
        if let Some(win) = e.wm.window_mut(child) {
            win.needs_paint = true;
        }
    }
    Ok(hwnd)
}

fn load_dialog_template(e: &mut Emu, a: &Args, wide: bool) -> Res<Option<DlgTemplate>> {
    let Some(bytes) = crate::win32::kernel32_extra::resource_bytes(e, a[0], RT_DIALOG, a[1], wide)?
    else {
        e.win.last_error = 1814; // ERROR_RESOURCE_NAME_NOT_FOUND
        return Ok(None);
    };
    Ok(parse_dialog_template(&bytes))
}

/// CreateDialogParam: a modeless dialog; returns its handle immediately.
fn create_dialog(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let Some(t) = load_dialog_template(e, a, wide)? else {
        return Ok(0);
    };
    build_dialog(e, &t, a[2], a[3], a[4])
}

/// DialogBoxParam: modal. The dialog is created on the first call; until EndDialog
/// supplies a result the call keeps asking the host for input (`WaitForInput`) and is
/// retried, pumping the dialog's messages each time. The game's own message loop is
/// not involved, which is also how real Windows runs a modal dialog.
fn dialog_box(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    if e.wm.modal.is_none() {
        let Some(t) = load_dialog_template(e, a, wide)? else {
            return Ok(u32::MAX);
        };
        let hwnd = build_dialog(e, &t, a[2], a[3], a[4])?;
        e.wm.modal = Some(Modal { hwnd, result: None });
    }
    loop {
        let Some(Modal { hwnd, result }) = e.wm.modal.as_ref().map(|m| Modal {
            hwnd: m.hwnd,
            result: m.result,
        }) else {
            return Ok(0);
        };
        if let Some(r) = result {
            destroy_window(e, hwnd)?;
            e.wm.modal = None;
            return Ok(r as u32);
        }
        e.wm.now_ms = e.wm.now_ms.max(e.win.elapsed_ms());
        let Some(m) = e.wm.next_message(true) else {
            return Err(Stop::Blocked(crate::win32::threads::Block::Input));
        };
        modal_dispatch(e, hwnd, m)?;
    }
}

fn modal_dispatch(e: &mut Emu, dlg: u32, m: Msg) -> Res<()> {
    const VK_RETURN: u32 = 0x0D;
    const VK_ESCAPE: u32 = 0x1B;
    let in_dialog = m.hwnd == dlg || e.wm.window(m.hwnd).is_some_and(|w| w.parent == dlg);
    match m.message {
        WM_QUIT => {
            // The app is quitting: put the request back for its own loop and end the dialog.
            e.wm.quit = Some(m.wparam);
            if let Some(modal) = e.wm.modal.as_mut() {
                modal.result = Some(0);
            }
        }
        WM_KEYDOWN | WM_KEYUP | WM_CHAR | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEMOVE
            if !in_dialog && m.message != WM_KEYDOWN =>
        {
            // Input meant for the disabled owner window.
        }
        WM_KEYDOWN => match m.wparam {
            VK_RETURN => {
                send_message(e, dlg, WM_COMMAND, 1, 0)?; // IDOK
            }
            VK_ESCAPE => {
                send_message(e, dlg, WM_COMMAND, 2, 0)?; // IDCANCEL
            }
            _ => {
                send_message(e, m.hwnd, m.message, m.wparam, m.lparam)?;
            }
        },
        WM_TIMER if m.lparam != 0 => {
            let now = e.wm.now_ms as u32;
            e.call_function(m.lparam, &[m.hwnd, WM_TIMER, m.wparam, now])?;
        }
        _ if m.hwnd == 0 => {}
        _ => {
            send_message(e, m.hwnd, m.message, m.wparam, m.lparam)?;
            if m.message == WM_PAINT
                && let Some(win) = e.wm.window_mut(m.hwnd)
            {
                win.needs_paint = false;
            }
        }
    }
    Ok(())
}

fn set_dlg_item_text(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let Some(h) = dlg_item(e, a[0], a[1]) else {
        return Ok(0);
    };
    let t = read_str(e, a[2], wide)?;
    if let Some(w) = e.wm.window_mut(h) {
        w.title = t;
    }
    Ok(1)
}
