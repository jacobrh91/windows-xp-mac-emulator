//! Software framebuffer: the target that GDI/DirectDraw-style calls will draw into
//! before a Metal presenter exists.

pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    /// 0x00RRGGBB
    pub pixels: Vec<u32>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        Framebuffer { width, height, pixels: vec![0; (width * height) as usize] }
    }

    pub fn set_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height {
            self.pixels[(y as u32 * self.width + x as u32) as usize] = color;
        }
    }

    pub fn get_pixel(&self, x: u32, y: u32) -> Option<u32> {
        (x < self.width && y < self.height).then(|| self.pixels[(y * self.width + x) as usize])
    }

    /// Fill the rectangle at (x, y), clipped to the framebuffer.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: u32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x as i64 + w as i64).min(self.width as i64) as i32;
        let y1 = (y as i64 + h as i64).min(self.height as i64) as i32;
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.pixels[(yy as u32 * self.width + xx as u32) as usize] = color;
            }
        }
    }

    /// Copy `src` onto this framebuffer with its top-left at (x, y), clipped.
    pub fn blit(&mut self, src: &Framebuffer, x: i32, y: i32) {
        for sy in 0..src.height {
            for sx in 0..src.width {
                self.set_pixel(x + sx as i32, y + sy as i32, src.pixels[(sy * src.width + sx) as usize]);
            }
        }
    }

    /// Binary PPM (P6), handy for eyeballing output without a window system.
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        for p in &self.pixels {
            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
        }
        out
    }
}

impl Framebuffer {
    /// 1px rectangle outline.
    pub fn frame_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.fill_rect(x, y, w, 1, color);
        self.fill_rect(x, y + h as i32 - 1, w, 1, color);
        self.fill_rect(x, y, 1, h, color);
        self.fill_rect(x + w as i32 - 1, y, 1, h, color);
    }

    /// Bresenham line, 1px.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.set_pixel(x, y, color);
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
    }

    /// Copy the `w`x`h` region at (x, y) into a new framebuffer (out-of-range reads as 0).
    pub fn crop(&self, x: i32, y: i32, w: u32, h: u32) -> Framebuffer {
        let mut out = Framebuffer::new(w, h);
        for yy in 0..h {
            for xx in 0..w {
                let (sx, sy) = (x + xx as i32, y + yy as i32);
                if sx >= 0 && sy >= 0 {
                    out.pixels[(yy * w + xx) as usize] = self.get_pixel(sx as u32, sy as u32).unwrap_or(0);
                }
            }
        }
        out
    }

    /// Nearest-neighbour resize.
    pub fn scaled(&self, w: u32, h: u32) -> Framebuffer {
        let mut out = Framebuffer::new(w, h);
        if self.width == 0 || self.height == 0 {
            return out;
        }
        for y in 0..h {
            let sy = (y as u64 * self.height as u64 / h as u64) as u32;
            for x in 0..w {
                let sx = (x as u64 * self.width as u64 / w as u64) as u32;
                out.pixels[(y * w + x) as usize] = self.pixels[(sy * self.width + sx) as usize];
            }
        }
        out
    }

    /// Decode a packed DIB (BITMAPINFOHEADER + palette + pixels), as stored in
    /// RT_BITMAP resources. Supports uncompressed 1/4/8/24/32-bit images.
    pub fn from_dib(dib: &[u8]) -> Option<Framebuffer> {
        let rd32 = |o: usize| dib.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let rd16 = |o: usize| dib.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let header = rd32(0)? as usize;
        // OS/2-style BITMAPCOREHEADER: 16-bit dimensions and 3-byte palette entries.
        let core = header == 12;
        let (width, height, bpp, compression, clr_used) = if core {
            (rd16(4)? as i32, rd16(6)? as i32, rd16(10)? as usize, 0, 0)
        } else {
            (rd32(4)? as i32, rd32(8)? as i32, rd16(14)? as usize, rd32(16)?, rd32(32)? as usize)
        };
        if (!core && header < 40) || width <= 0 || height == 0 || compression != 0 || width > 8192 || height.abs() > 8192 {
            return None;
        }
        let palette_len = match bpp {
            1 | 4 | 8 => if clr_used != 0 { clr_used } else { 1 << bpp },
            24 | 32 => 0,
            _ => return None,
        };
        let entry = if core { 3 } else { 4 };
        let palette: Vec<u32> = (0..palette_len)
            .map(|i| {
                let o = header + entry * i;
                dib.get(o..o + 3).map_or(0, |c| ((c[2] as u32) << 16) | ((c[1] as u32) << 8) | c[0] as u32)
            })
            .collect();
        let bits = header + entry * palette_len;
        let stride = (width as usize * bpp).div_ceil(32) * 4;
        let (w, h) = (width as u32, height.unsigned_abs());
        let mut fb = Framebuffer::new(w, h);
        for row in 0..h as usize {
            let src_row = if height > 0 { h as usize - 1 - row } else { row }; // bottom-up by default
            let base = bits + src_row * stride;
            let line = dib.get(base..base + stride.min(dib.len().saturating_sub(base)))?;
            for x in 0..w as usize {
                let px = match bpp {
                    1 => palette.get(((*line.get(x / 8)? >> (7 - x % 8)) & 1) as usize).copied()?,
                    4 => palette.get(((*line.get(x / 2)? >> (if x % 2 == 0 { 4 } else { 0 })) & 15) as usize).copied()?,
                    8 => palette.get(*line.get(x)? as usize).copied()?,
                    24 => {
                        let p = line.get(3 * x..3 * x + 3)?;
                        ((p[2] as u32) << 16) | ((p[1] as u32) << 8) | p[0] as u32
                    }
                    _ => {
                        let p = line.get(4 * x..4 * x + 3)?;
                        ((p[2] as u32) << 16) | ((p[1] as u32) << 8) | p[0] as u32
                    }
                };
                fb.pixels[row * w as usize + x] = px;
            }
        }
        Some(fb)
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: u32, alpha: u8) {
        if let Some(dst) = self.get_pixel(x.max(0) as u32, y.max(0) as u32).filter(|_| x >= 0 && y >= 0) {
            let a = alpha as u32;
            let mix = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
            let r = mix((color >> 16) & 0xff, (dst >> 16) & 0xff);
            let g = mix((color >> 8) & 0xff, (dst >> 8) & 0xff);
            let b = mix(color & 0xff, dst & 0xff);
            self.set_pixel(x, y, (r << 16) | (g << 8) | b);
        }
    }
}

/// A TrueType font from the host system. Text still lays out (with an estimated
/// fixed advance) when no font could be loaded, so callers never need to special-case it.
pub struct Font {
    inner: Option<fontdue::Font>,
}

impl Font {
    pub fn load_system() -> Font {
        const CANDIDATES: &[&str] = &[
            "/System/Library/Fonts/Geneva.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/Library/Fonts/Arial.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ];
        let inner = CANDIDATES.iter().find_map(|p| {
            let bytes = std::fs::read(p).ok()?;
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
        });
        Font { inner }
    }

    #[cfg(test)]
    pub fn none() -> Font {
        Font { inner: None }
    }

    #[cfg(test)]
    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }

    /// Width and height in pixels of a single line of text.
    pub fn measure(&self, text: &str, px: u32) -> (u32, u32) {
        match &self.inner {
            Some(f) => {
                let w: f32 = text.chars().map(|c| f.metrics(c, px as f32).advance_width).sum();
                (w.ceil() as u32, px + px / 4)
            }
            None => (text.chars().count() as u32 * (px * 6 / 10).max(1), px + px / 4),
        }
    }

    /// Draw `text` with its top-left at (x, y).
    pub fn draw(&self, fb: &mut Framebuffer, x: i32, y: i32, text: &str, px: u32, color: u32) {
        let Some(f) = &self.inner else { return };
        let ascent = f.horizontal_line_metrics(px as f32).map(|m| m.ascent).unwrap_or(px as f32 * 0.8);
        let baseline = y + ascent.round() as i32;
        let mut pen = x as f32;
        for c in text.chars() {
            let (m, bitmap) = f.rasterize(c, px as f32);
            let gx = pen.round() as i32 + m.xmin;
            let gy = baseline - m.ymin - m.height as i32;
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = bitmap[row * m.width + col];
                    if a > 0 {
                        fb.blend_pixel(gx + col as i32, gy + row as i32, color, a);
                    }
                }
            }
            pen += m.advance_width;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rect_is_clipped() {
        let mut fb = Framebuffer::new(4, 4);
        fb.fill_rect(-2, -2, 4, 4, 0xff0000);
        assert_eq!(fb.get_pixel(0, 0), Some(0xff0000));
        assert_eq!(fb.get_pixel(1, 1), Some(0xff0000));
        assert_eq!(fb.get_pixel(2, 2), Some(0));
        fb.fill_rect(3, 3, 100, 100, 1);
        assert_eq!(fb.get_pixel(3, 3), Some(1));
    }

    #[test]
    fn blit_clips_and_copies() {
        let mut dst = Framebuffer::new(3, 3);
        let mut src = Framebuffer::new(2, 2);
        src.fill_rect(0, 0, 2, 2, 0x123456);
        dst.blit(&src, 2, 2);
        assert_eq!(dst.get_pixel(2, 2), Some(0x123456));
        assert_eq!(dst.get_pixel(1, 1), Some(0));
    }

    /// A packed DIB header for a `w` x `h` image.
    fn dib_header(w: i32, h: i32, bpp: u16, colors: u32) -> Vec<u8> {
        let mut d = Vec::new();
        for v in [40u32, w as u32, h as u32] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&bpp.to_le_bytes());
        for v in [0u32, 0, 0, 0, colors, 0] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d
    }

    #[test]
    fn dib_24bit_is_bottom_up_bgr_with_row_padding() {
        let mut d = dib_header(1, 2, 24, 0);
        d.extend_from_slice(&[0, 0, 255, 0]); // bottom row: red (BGR) + 1 byte padding
        d.extend_from_slice(&[255, 0, 0, 0]); // top row: blue
        let fb = Framebuffer::from_dib(&d).unwrap();
        assert_eq!((fb.width, fb.height), (1, 2));
        assert_eq!(fb.get_pixel(0, 0), Some(0x0000FF), "first stored row is the bottom row");
        assert_eq!(fb.get_pixel(0, 1), Some(0xFF0000));
    }

    #[test]
    fn dib_8bit_uses_palette_and_top_down_when_height_negative() {
        let mut d = dib_header(2, -1, 8, 2);
        d.extend_from_slice(&[0, 0, 0, 0, 10, 20, 30, 0]); // palette: black, then BGR=(10,20,30)
        d.extend_from_slice(&[0, 1, 0, 0]);
        let fb = Framebuffer::from_dib(&d).unwrap();
        assert_eq!(fb.get_pixel(0, 0), Some(0));
        assert_eq!(fb.get_pixel(1, 0), Some(0x1E140A));
    }

    #[test]
    fn dib_4bit_and_1bit_unpack_pixels() {
        let mut d = dib_header(2, 1, 4, 2);
        d.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0, ]);
        d.extend_from_slice(&[0x01, 0, 0, 0]);
        let fb = Framebuffer::from_dib(&d).unwrap();
        assert_eq!((fb.get_pixel(0, 0), fb.get_pixel(1, 0)), (Some(0), Some(0xFFFFFF)));

        let mut d = dib_header(8, 1, 1, 2);
        d.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        d.extend_from_slice(&[0b1010_0000, 0, 0, 0]);
        let fb = Framebuffer::from_dib(&d).unwrap();
        let row: Vec<u32> = fb.pixels.clone();
        assert_eq!(&row[..4], &[0xFFFFFF, 0, 0xFFFFFF, 0]);
    }

    #[test]
    fn dib_core_header_uses_three_byte_palette() {
        // BITMAPCOREHEADER: size 12, w=2, h=1, planes=1, bpp=1
        let mut d = vec![12, 0, 0, 0, 2, 0, 1, 0, 1, 0, 1, 0];
        d.extend_from_slice(&[0, 0, 0, 255, 0, 0]); // black, then BGR=(255,0,0) = blue
        d.extend_from_slice(&[0b0100_0000, 0, 0, 0]);
        let fb = Framebuffer::from_dib(&d).unwrap();
        assert_eq!((fb.get_pixel(0, 0), fb.get_pixel(1, 0)), (Some(0), Some(0x0000FF)));
    }

    #[test]
    fn dib_rejects_garbage_and_truncation() {
        assert!(Framebuffer::from_dib(&[]).is_none());
        assert!(Framebuffer::from_dib(&[0; 10]).is_none());
        let mut d = dib_header(4, 4, 24, 0);
        d.extend_from_slice(&[0; 10]); // far too little pixel data
        assert!(Framebuffer::from_dib(&d).is_none());
        let mut compressed = dib_header(2, 2, 8, 0);
        compressed[16] = 1; // BI_RLE8
        assert!(Framebuffer::from_dib(&compressed).is_none());
    }

    #[test]
    fn scaling_up_and_down() {
        let mut fb = Framebuffer::new(2, 2);
        fb.set_pixel(1, 0, 7);
        let big = fb.scaled(4, 4);
        assert_eq!((big.get_pixel(2, 0), big.get_pixel(3, 1), big.get_pixel(0, 0)), (Some(7), Some(7), Some(0)));
        let small = big.scaled(1, 1);
        assert_eq!(small.get_pixel(0, 0), Some(0));
        assert_eq!(Framebuffer::new(0, 0).scaled(3, 3).pixels, vec![0; 9]);
    }

    #[test]
    fn lines_and_frames() {
        let mut fb = Framebuffer::new(8, 8);
        fb.line(0, 0, 7, 7, 1);
        assert!((0..8).all(|i| fb.get_pixel(i, i) == Some(1)));
        fb.frame_rect(0, 0, 8, 8, 2);
        assert_eq!(fb.get_pixel(7, 3), Some(2));
        assert_eq!(fb.get_pixel(3, 5), Some(0));
        let mut h = Framebuffer::new(5, 1);
        h.line(4, 0, 0, 0, 3);
        assert!(h.pixels.iter().all(|&p| p == 3), "lines draw right-to-left too");
    }

    #[test]
    fn crop_copies_and_pads() {
        let mut fb = Framebuffer::new(4, 4);
        fb.fill_rect(2, 2, 2, 2, 9);
        let c = fb.crop(2, 2, 3, 3);
        assert_eq!(c.get_pixel(0, 0), Some(9));
        assert_eq!(c.get_pixel(2, 2), Some(0), "outside the source reads as 0");
    }

    #[test]
    fn font_measure_works_without_a_loaded_font() {
        let f = Font::none();
        let (w, h) = f.measure("abcd", 10);
        assert_eq!((w, h), (24, 12));
        let mut fb = Framebuffer::new(10, 10);
        f.draw(&mut fb, 0, 0, "abcd", 10, 0xffffff); // must not panic
        assert!(fb.pixels.iter().all(|&p| p == 0));
    }

    #[test]
    fn system_font_draws_ink_when_available() {
        let f = Font::load_system();
        if !f.is_loaded() {
            return; // no fonts on this machine
        }
        let mut fb = Framebuffer::new(60, 20);
        f.draw(&mut fb, 2, 2, "Hello", 13, 0xffffff);
        assert!(fb.pixels.iter().any(|&p| p != 0), "text left some pixels");
        assert!(f.measure("Hello", 13).0 > f.measure("H", 13).0);
    }

    #[test]
    fn out_of_range_pixel_is_none() {
        assert_eq!(Framebuffer::new(2, 2).get_pixel(2, 0), None);
    }

    #[test]
    fn ppm_header_and_size() {
        let mut fb = Framebuffer::new(2, 1);
        fb.set_pixel(0, 0, 0x010203);
        let ppm = fb.to_ppm();
        assert!(ppm.starts_with(b"P6\n2 1\n255\n"));
        assert_eq!(&ppm[ppm.len() - 6..], &[1, 2, 3, 0, 0, 0]);
    }
}
