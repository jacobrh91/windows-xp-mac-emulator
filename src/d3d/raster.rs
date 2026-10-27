//! A small software rasterizer implementing the Direct3D 8 fixed-function pixel pipeline:
//! perspective-correct triangles, depth test, alpha test and blend, two texture stages, and
//! texture sampling (including DXT-compressed formats). It is the reference renderer; a GPU
//! backend can be checked against it.

use crate::memory::Memory;

// ---- D3DFORMAT ---------------------------------------------------------------------------
pub const FMT_R8G8B8: u32 = 20;
pub const FMT_A8R8G8B8: u32 = 21;
pub const FMT_X8R8G8B8: u32 = 22;
pub const FMT_R5G6B5: u32 = 23;
pub const FMT_X1R5G5B5: u32 = 24;
pub const FMT_A1R5G5B5: u32 = 25;
pub const FMT_A4R4G4B4: u32 = 26;
pub const FMT_A8: u32 = 28;
pub const FMT_X4R4G4B4: u32 = 30;
pub const FMT_L8: u32 = 50;
pub const FMT_A8L8: u32 = 51;
pub const FMT_DXT1: u32 = 0x3154_5844;
pub const FMT_DXT2: u32 = 0x3254_5844;
pub const FMT_DXT3: u32 = 0x3354_5844;
pub const FMT_DXT4: u32 = 0x3454_5844;
pub const FMT_DXT5: u32 = 0x3554_5844;

pub fn is_dxt(f: u32) -> bool {
    matches!(f, FMT_DXT1..=FMT_DXT5)
}

/// Bytes per pixel for uncompressed formats (0 for DXT).
pub fn bytes_per_pixel(f: u32) -> u32 {
    match f {
        FMT_A8R8G8B8 | FMT_X8R8G8B8 => 4,
        FMT_R8G8B8 => 3,
        FMT_R5G6B5 | FMT_X1R5G5B5 | FMT_A1R5G5B5 | FMT_A4R4G4B4 | FMT_X4R4G4B4 | FMT_A8L8 => 2,
        FMT_A8 | FMT_L8 => 1,
        _ => 0,
    }
}

/// (pitch in bytes, total bytes) of a `w` x `h` surface in format `f`.
pub fn surface_size(f: u32, w: u32, h: u32) -> (u32, u32) {
    if is_dxt(f) {
        let block = if f == FMT_DXT1 { 8 } else { 16 };
        let (bw, bh) = (w.div_ceil(4).max(1), h.div_ceil(4).max(1));
        (bw * block, bw * bh * block)
    } else {
        let pitch = w * bytes_per_pixel(f).max(1);
        (pitch, pitch * h)
    }
}

// ---- state ---------------------------------------------------------------------------------
pub const CMP_NEVER: u32 = 1;
pub const CMP_LESS: u32 = 2;
pub const CMP_EQUAL: u32 = 3;
pub const CMP_LESSEQUAL: u32 = 4;
pub const CMP_GREATER: u32 = 5;
pub const CMP_NOTEQUAL: u32 = 6;
pub const CMP_GREATEREQUAL: u32 = 7;
pub const CMP_ALWAYS: u32 = 8;

pub fn compare(func: u32, a: f32, b: f32) -> bool {
    match func {
        CMP_NEVER => false,
        CMP_LESS => a < b,
        CMP_EQUAL => a == b,
        CMP_LESSEQUAL => a <= b,
        CMP_GREATER => a > b,
        CMP_NOTEQUAL => a != b,
        CMP_GREATEREQUAL => a >= b,
        _ => true,
    }
}

#[derive(Clone, Debug)]
pub struct RenderState {
    pub zenable: bool,
    pub zwrite: bool,
    pub zfunc: u32,
    pub alpha_blend: bool,
    pub src_blend: u32,
    pub dst_blend: u32,
    pub alpha_test: bool,
    pub alpha_ref: u32,
    pub alpha_func: u32,
    /// 1 none, 2 cull clockwise, 3 cull counter-clockwise.
    pub cull: u32,
    pub texture_factor: u32,
    pub color_write: u32,
    pub specular: bool,
    /// Pixels outside this rectangle (x0, y0, x1, y1; exclusive end) are left alone: the viewport.
    pub scissor: [u32; 4],
}

impl Default for RenderState {
    fn default() -> Self {
        RenderState {
            zenable: true,
            zwrite: true,
            zfunc: CMP_LESSEQUAL,
            alpha_blend: false,
            src_blend: 2,
            dst_blend: 1,
            alpha_test: false,
            alpha_ref: 0,
            alpha_func: CMP_ALWAYS,
            cull: 3,
            texture_factor: 0xFFFF_FFFF,
            color_write: 0xF,
            specular: false,
            scissor: [0, 0, u32::MAX, u32::MAX],
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StageState {
    pub color_op: u32,
    pub color_arg1: u32,
    pub color_arg2: u32,
    pub alpha_op: u32,
    pub alpha_arg1: u32,
    pub alpha_arg2: u32,
    pub addr_u: u32,
    pub addr_v: u32,
    pub mag: u32,
    pub min: u32,
    pub texcoord_index: u32,
}

impl StageState {
    pub fn default_for(stage: usize) -> Self {
        StageState {
            color_op: if stage == 0 { 4 } else { 1 }, // MODULATE, else DISABLE
            color_arg1: 2,                            // TEXTURE
            color_arg2: 1,                            // CURRENT
            alpha_op: if stage == 0 { 2 } else { 1 }, // SELECTARG1, else DISABLE
            alpha_arg1: 2,
            alpha_arg2: 1,
            addr_u: 1,
            addr_v: 1,
            mag: 1,
            min: 1,
            texcoord_index: stage as u32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Level {
    pub ptr: u32,
    pub w: u32,
    pub h: u32,
    pub pitch: u32,
}

#[derive(Clone, Debug)]
pub struct TextureRef {
    pub format: u32,
    pub levels: Vec<Level>,
}

/// A vertex after transformation: pixel coordinates, depth in 0..1, and 1/w.
#[derive(Clone, Copy, Debug, Default)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub rhw: f32,
    pub color: [f32; 4],
    pub spec: [f32; 4],
    pub uv: [[f32; 2]; 2],
}

pub struct Target {
    pub w: u32,
    pub h: u32,
    pub color: Vec<u32>,
    pub depth: Vec<f32>,
}

impl Target {
    pub fn new(w: u32, h: u32) -> Self {
        Target {
            w,
            h,
            color: vec![0; (w * h) as usize],
            depth: vec![1.0; (w * h) as usize],
        }
    }

    pub fn clear(&mut self, color: Option<u32>, depth: Option<f32>) {
        if let Some(c) = color {
            self.color.fill(c);
        }
        if let Some(z) = depth {
            self.depth.fill(z);
        }
    }
}

// ---- color helpers -------------------------------------------------------------------------
pub fn unpack(c: u32) -> [f32; 4] {
    [
        ((c >> 16) & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        (c & 0xFF) as f32 / 255.0,
        ((c >> 24) & 0xFF) as f32 / 255.0,
    ]
}

pub fn pack(c: [f32; 4]) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    (q(c[3]) << 24) | (q(c[0]) << 16) | (q(c[1]) << 8) | q(c[2])
}

fn expand(v: u32, bits: u32) -> f32 {
    v as f32 / ((1u32 << bits) - 1) as f32
}

fn rgb565(c: u16) -> [f32; 3] {
    [
        expand(((c >> 11) & 31) as u32, 5),
        expand(((c >> 5) & 63) as u32, 6),
        expand((c & 31) as u32, 5),
    ]
}

// ---- texture decoding ------------------------------------------------------------------------

/// The 4x4 block's colors: (RGB for each of the 4 palette entries, is 1-bit-alpha mode).
fn dxt_color_palette(c0: u16, c1: u16, four_color: bool) -> [[f32; 4]; 4] {
    let (a, b) = (rgb565(c0), rgb565(c1));
    let mix = |t: f32| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
            1.0,
        ]
    };
    if four_color {
        [
            [a[0], a[1], a[2], 1.0],
            [b[0], b[1], b[2], 1.0],
            mix(1.0 / 3.0),
            mix(2.0 / 3.0),
        ]
    } else {
        [
            [a[0], a[1], a[2], 1.0],
            [b[0], b[1], b[2], 1.0],
            mix(0.5),
            [0.0, 0.0, 0.0, 0.0],
        ]
    }
}

/// Decode texel (x, y) of a DXT surface.
fn decode_dxt(mem: &Memory, level: &Level, format: u32, x: u32, y: u32) -> [f32; 4] {
    let block_size = if format == FMT_DXT1 { 8 } else { 16 };
    let (bx, by) = (x / 4, y / 4);
    let base = level.ptr + by * level.pitch + bx * block_size;
    let (cx, cy) = (x % 4, y % 4);
    let color_at = if format == FMT_DXT1 { base } else { base + 8 };
    let c0 = mem.read(color_at, 2).unwrap_or(0) as u16;
    let c1 = mem.read(color_at + 2, 2).unwrap_or(0) as u16;
    let bits = mem.read(color_at + 4, 4).unwrap_or(0);
    let palette = dxt_color_palette(c0, c1, format != FMT_DXT1 || c0 > c1);
    let mut px = palette[((bits >> (2 * (cy * 4 + cx))) & 3) as usize];
    match format {
        FMT_DXT2 | FMT_DXT3 => {
            let row = mem.read(base + cy * 2, 2).unwrap_or(0);
            px[3] = expand((row >> (4 * cx)) & 15, 4);
        }
        FMT_DXT4 | FMT_DXT5 => {
            let (a0, a1) = (
                mem.read_u8(base).unwrap_or(0) as f32,
                mem.read_u8(base + 1).unwrap_or(0) as f32,
            );
            let lo = mem.read(base + 2, 4).unwrap_or(0) as u64;
            let hi = mem.read(base + 6, 2).unwrap_or(0) as u64;
            let idx = (((hi << 32) | lo) >> (3 * (cy * 4 + cx))) & 7;
            let a = if a0 > a1 {
                match idx {
                    0 => a0,
                    1 => a1,
                    n => ((8 - n) as f32 * a0 + (n - 1) as f32 * a1) / 7.0,
                }
            } else {
                match idx {
                    0 => a0,
                    1 => a1,
                    6 => 0.0,
                    7 => 255.0,
                    n => ((6 - n) as f32 * a0 + (n - 1) as f32 * a1) / 5.0,
                }
            };
            px[3] = a / 255.0;
        }
        _ => {}
    }
    px
}

/// Read one texel as RGBA in 0..1.
pub fn texel(mem: &Memory, format: u32, level: &Level, x: u32, y: u32) -> [f32; 4] {
    let (x, y) = (
        x.min(level.w.saturating_sub(1)),
        y.min(level.h.saturating_sub(1)),
    );
    if is_dxt(format) {
        return decode_dxt(mem, level, format, x, y);
    }
    let bpp = bytes_per_pixel(format);
    let addr = level.ptr + y * level.pitch + x * bpp;
    let raw = mem.read(addr, bpp.max(1).min(4)).unwrap_or(0);
    match format {
        FMT_A8R8G8B8 => unpack(raw),
        FMT_X8R8G8B8 => {
            let mut c = unpack(raw);
            c[3] = 1.0;
            c
        }
        FMT_R8G8B8 => {
            let b = mem.read_u8(addr).unwrap_or(0) as f32 / 255.0;
            let g = mem.read_u8(addr + 1).unwrap_or(0) as f32 / 255.0;
            let r = mem.read_u8(addr + 2).unwrap_or(0) as f32 / 255.0;
            [r, g, b, 1.0]
        }
        FMT_R5G6B5 => {
            let c = rgb565(raw as u16);
            [c[0], c[1], c[2], 1.0]
        }
        FMT_X1R5G5B5 | FMT_A1R5G5B5 => {
            let a = if format == FMT_A1R5G5B5 {
                (raw >> 15) as f32
            } else {
                1.0
            };
            [
                expand((raw >> 10) & 31, 5),
                expand((raw >> 5) & 31, 5),
                expand(raw & 31, 5),
                a,
            ]
        }
        FMT_A4R4G4B4 | FMT_X4R4G4B4 => {
            let a = if format == FMT_A4R4G4B4 {
                expand((raw >> 12) & 15, 4)
            } else {
                1.0
            };
            [
                expand((raw >> 8) & 15, 4),
                expand((raw >> 4) & 15, 4),
                expand(raw & 15, 4),
                a,
            ]
        }
        FMT_A8 => [1.0, 1.0, 1.0, raw as f32 / 255.0],
        FMT_L8 => {
            let l = raw as f32 / 255.0;
            [l, l, l, 1.0]
        }
        FMT_A8L8 => {
            let l = (raw & 0xFF) as f32 / 255.0;
            [l, l, l, (raw >> 8) as f32 / 255.0]
        }
        _ => [1.0, 0.0, 1.0, 1.0], // unknown format: magenta, so it is noticed
    }
}

fn wrap_coord(v: f32, size: u32, mode: u32) -> f32 {
    let s = size as f32;
    match mode {
        2 => {
            // mirror
            let t = v.rem_euclid(2.0);
            (if t > 1.0 { 2.0 - t } else { t }) * s
        }
        3 | 4 => v.clamp(0.0, 1.0) * s, // clamp (and border, approximated)
        _ => v.rem_euclid(1.0) * s,     // wrap
    }
}

/// Sample a texture (level 0) at normalised coordinates.
pub fn sample(mem: &Memory, tex: &TextureRef, st: &StageState, u: f32, v: f32) -> [f32; 4] {
    let Some(level) = tex.levels.first() else {
        return [1.0, 1.0, 1.0, 1.0];
    };
    let (fx, fy) = (
        wrap_coord(u, level.w, st.addr_u),
        wrap_coord(v, level.h, st.addr_v),
    );
    if st.mag == 1 {
        return texel(
            mem,
            tex.format,
            level,
            (fx as u32).min(level.w - 1),
            (fy as u32).min(level.h - 1),
        );
    }
    // bilinear
    let (fx, fy) = (fx - 0.5, fy - 0.5);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let wrap_i = |i: f32, n: u32, mode: u32| -> u32 {
        let i = i as i64;
        if mode == 1 {
            i.rem_euclid(n as i64) as u32
        } else {
            i.clamp(0, n as i64 - 1) as u32
        }
    };
    let (xa, xb) = (
        wrap_i(x0, level.w, st.addr_u),
        wrap_i(x0 + 1.0, level.w, st.addr_u),
    );
    let (ya, yb) = (
        wrap_i(y0, level.h, st.addr_v),
        wrap_i(y0 + 1.0, level.h, st.addr_v),
    );
    let (c00, c10) = (
        texel(mem, tex.format, level, xa, ya),
        texel(mem, tex.format, level, xb, ya),
    );
    let (c01, c11) = (
        texel(mem, tex.format, level, xa, yb),
        texel(mem, tex.format, level, xb, yb),
    );
    let mut out = [0.0; 4];
    for i in 0..4 {
        let top = c00[i] + (c10[i] - c00[i]) * tx;
        let bot = c01[i] + (c11[i] - c01[i]) * tx;
        out[i] = top + (bot - top) * ty;
    }
    out
}

// ---- texture stage combiners -----------------------------------------------------------------

struct Inputs {
    diffuse: [f32; 4],
    specular: [f32; 4],
    texture: [f32; 4],
    tfactor: [f32; 4],
}

fn pick(arg: u32, current: [f32; 4], i: &Inputs) -> [f32; 4] {
    let base = match arg & 0xF {
        0 => i.diffuse,
        1 => current,
        2 => i.texture,
        3 => i.tfactor,
        4 => i.specular,
        _ => current,
    };
    let mut v = base;
    if arg & 0x10 != 0 {
        for c in v.iter_mut() {
            *c = 1.0 - *c; // D3DTA_COMPLEMENT
        }
    }
    if arg & 0x20 != 0 {
        v[0] = v[3]; // D3DTA_ALPHAREPLICATE
        v[1] = v[3];
        v[2] = v[3];
    }
    v
}

fn apply_op(
    op: u32,
    a1: [f32; 4],
    a2: [f32; 4],
    cur: [f32; 4],
    i: &Inputs,
    channels: usize,
) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    for c in 0..channels {
        let (x, y) = (a1[c], a2[c]);
        out[c] = match op {
            2 => x,
            3 => y,
            4 => x * y,
            5 => x * y * 2.0,
            6 => x * y * 4.0,
            7 => x + y,
            8 => x + y - 0.5,
            9 => (x + y - 0.5) * 2.0,
            10 => x - y,
            11 => x + y - x * y,
            12 => x * i.diffuse[3] + y * (1.0 - i.diffuse[3]),
            13 => x * i.texture[3] + y * (1.0 - i.texture[3]),
            14 => x * i.tfactor[3] + y * (1.0 - i.tfactor[3]),
            15 => x + y * (1.0 - i.texture[3]),
            16 => x * cur[3] + y * (1.0 - cur[3]),
            _ => x,
        };
    }
    out
}

/// Run the (up to two) texture stages and produce the pixel's color before blending.
fn shade(
    mem: &Memory,
    st: &RenderState,
    stages: &[StageState; 2],
    tex: &[Option<TextureRef>; 2],
    v: &Vertex,
) -> [f32; 4] {
    let mut current = v.color;
    let tfactor = unpack(st.texture_factor);
    for (s, stage) in stages.iter().enumerate() {
        if stage.color_op == 1 {
            break; // D3DTOP_DISABLE ends the cascade
        }
        // A stage that reads its texture but has none bound is disabled, ending the cascade.
        let reads_texture = [
            stage.color_arg1,
            stage.color_arg2,
            stage.alpha_arg1,
            stage.alpha_arg2,
        ]
        .iter()
        .any(|a| a & 0xF == 2);
        if tex[s].is_none() && reads_texture {
            break;
        }
        let uv = v.uv[(stage.texcoord_index as usize & 0xFFFF).min(1)];
        let texture = match &tex[s] {
            Some(t) => sample(mem, t, stage, uv[0], uv[1]),
            None => [1.0, 1.0, 1.0, 1.0],
        };
        let inputs = Inputs {
            diffuse: v.color,
            specular: v.spec,
            texture,
            tfactor,
        };
        let (c1, c2) = (
            pick(stage.color_arg1, current, &inputs),
            pick(stage.color_arg2, current, &inputs),
        );
        let rgb = apply_op(stage.color_op, c1, c2, current, &inputs, 3);
        let alpha = if stage.alpha_op == 1 {
            current[3]
        } else {
            let (a1, a2) = (
                pick(stage.alpha_arg1, current, &inputs),
                pick(stage.alpha_arg2, current, &inputs),
            );
            apply_op(stage.alpha_op, a1, a2, current, &inputs, 4)[3]
        };
        current = [rgb[0], rgb[1], rgb[2], alpha];
    }
    if st.specular {
        for c in 0..3 {
            current[c] += v.spec[c];
        }
    }
    current
}

fn blend_factor(f: u32, src: [f32; 4], dst: [f32; 4]) -> [f32; 3] {
    match f {
        1 => [0.0; 3],
        2 => [1.0; 3],
        3 => [src[0], src[1], src[2]],
        4 => [1.0 - src[0], 1.0 - src[1], 1.0 - src[2]],
        5 => [src[3]; 3],
        6 => [1.0 - src[3]; 3],
        7 => [dst[3]; 3],
        8 => [1.0 - dst[3]; 3],
        9 => [dst[0], dst[1], dst[2]],
        10 => [1.0 - dst[0], 1.0 - dst[1], 1.0 - dst[2]],
        11 => {
            let k = src[3].min(1.0 - dst[3]);
            [k; 3]
        }
        _ => [1.0; 3],
    }
}

// ---- triangle rasterization -----------------------------------------------------------------

fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (bx - ax) * (py - ay) - (by - ay) * (px - ax)
}

/// Draw one triangle (vertices already in pixel space).
pub fn draw_triangle(
    target: &mut Target,
    mem: &Memory,
    st: &RenderState,
    stages: &[StageState; 2],
    tex: &[Option<TextureRef>; 2],
    v: [&Vertex; 3],
) {
    let (a, b, c) = (v[0], v[1], v[2]);
    let area = edge(a.x, a.y, b.x, b.y, c.x, c.y);
    if area == 0.0 || !area.is_finite() {
        return;
    }
    // Y grows downwards, so a positive area is clockwise on screen.
    let clockwise = area > 0.0;
    match st.cull {
        2 if clockwise => return,
        3 if !clockwise => return,
        _ => {}
    }
    let [sx0, sy0, sx1, sy1] = st.scissor;
    let (right, bottom) = (
        sx1.min(target.w) as f32 - 1.0,
        sy1.min(target.h) as f32 - 1.0,
    );
    let min_x = a.x.min(b.x).min(c.x).floor().max(sx0 as f32) as i32;
    let max_x = a.x.max(b.x).max(c.x).ceil().min(right) as i32;
    let min_y = a.y.min(b.y).min(c.y).floor().max(sy0 as f32) as i32;
    let max_y = a.y.max(b.y).max(c.y).ceil().min(bottom) as i32;
    if min_x > max_x || min_y > max_y {
        return;
    }
    let inv_area = 1.0 / area;
    let top_left = |p: &Vertex, q: &Vertex| {
        let (mut dx, mut dy) = (q.x - p.x, q.y - p.y);
        if !clockwise {
            (dx, dy) = (-dx, -dy);
        }
        (dy == 0.0 && dx > 0.0) || dy < 0.0
    };
    let tl = (top_left(b, c), top_left(c, a), top_left(a, b));
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let w0 = edge(b.x, b.y, c.x, c.y, px, py) * inv_area;
            let w1 = edge(c.x, c.y, a.x, a.y, px, py) * inv_area;
            let w2 = edge(a.x, a.y, b.x, b.y, px, py) * inv_area;
            // Pixels exactly on an edge belong to the triangle only for top and left edges, so
            // triangles sharing an edge never both draw (or both blend) the same pixel.
            if w0 < 0.0
                || w1 < 0.0
                || w2 < 0.0
                || (w0 == 0.0 && !tl.0)
                || (w1 == 0.0 && !tl.1)
                || (w2 == 0.0 && !tl.2)
            {
                continue;
            }
            let idx = (y as u32 * target.w + x as u32) as usize;
            let z = w0 * a.z + w1 * b.z + w2 * c.z;
            if st.zenable && !compare(st.zfunc, z, target.depth[idx]) {
                continue;
            }
            // perspective-correct attributes
            let (r0, r1, r2) = (w0 * a.rhw, w1 * b.rhw, w2 * c.rhw);
            let sum = r0 + r1 + r2;
            let (p0, p1, p2) = if sum.abs() > 1e-12 {
                (r0 / sum, r1 / sum, r2 / sum)
            } else {
                (w0, w1, w2)
            };
            let mix = |f: fn(&Vertex) -> f32| p0 * f(a) + p1 * f(b) + p2 * f(c);
            let mut pix = Vertex {
                color: [
                    mix(|v| v.color[0]),
                    mix(|v| v.color[1]),
                    mix(|v| v.color[2]),
                    mix(|v| v.color[3]),
                ],
                spec: [
                    mix(|v| v.spec[0]),
                    mix(|v| v.spec[1]),
                    mix(|v| v.spec[2]),
                    mix(|v| v.spec[3]),
                ],
                ..Vertex::default()
            };
            pix.uv[0] = [mix(|v| v.uv[0][0]), mix(|v| v.uv[0][1])];
            pix.uv[1] = [mix(|v| v.uv[1][0]), mix(|v| v.uv[1][1])];
            let mut color = shade(mem, st, stages, tex, &pix);
            if st.alpha_test
                && !compare(
                    st.alpha_func,
                    (color[3] * 255.0 + 0.5).floor(),
                    (st.alpha_ref & 0xFF) as f32,
                )
            {
                continue;
            }
            if st.alpha_blend {
                // X8R8G8B8 has no alpha: the destination's reads back as 1.
                let mut dst = unpack(target.color[idx]);
                dst[3] = 1.0;
                let (sf, df) = (
                    blend_factor(st.src_blend, color, dst),
                    blend_factor(st.dst_blend, color, dst),
                );
                for k in 0..3 {
                    color[k] = color[k] * sf[k] + dst[k] * df[k];
                }
            }
            if st.color_write & 0xF != 0 {
                color[3] = 1.0;
                target.color[idx] = pack(color);
            }
            if st.zenable && st.zwrite {
                target.depth[idx] = z;
            }
        }
    }
}

#[cfg(test)]
mod tests;
