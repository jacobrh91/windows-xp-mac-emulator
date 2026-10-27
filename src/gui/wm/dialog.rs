//! Dialog templates, text wrapping and drawing the built-in controls.

use super::*;

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
pub(super) const CTRL_PX: u32 = 11;

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
pub(super) fn draw_wrapped(
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

pub(super) fn render_builtin(w: &mut Window, font: &Font) {
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
