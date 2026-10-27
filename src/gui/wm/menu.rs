//! Menus: reading resource templates, and laying out and drawing the menu bar and popups.

use super::*;

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

pub(super) fn menu_label(text: &str) -> (String, String) {
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

pub(super) fn draw_menu_bar(
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

pub(super) fn draw_popup(fb: &mut Framebuffer, font: &Font, menu: &Menu, x: i32, y: i32) {
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
