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
    let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as i32;
    let max_x = a.x.max(b.x).max(c.x).ceil().min(target.w as f32 - 1.0) as i32;
    let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as i32;
    let max_y = a.y.max(b.y).max(c.y).ceil().min(target.h as f32 - 1.0) as i32;
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
                let dst = unpack(target.color[idx]);
                let (sf, df) = (
                    blend_factor(st.src_blend, color, dst),
                    blend_factor(st.dst_blend, color, dst),
                );
                for k in 0..3 {
                    color[k] = color[k] * sf[k] + dst[k] * df[k];
                }
            }
            if st.color_write & 0xF != 0 {
                target.color[idx] = pack(color);
            }
            if st.zenable && st.zwrite {
                target.depth[idx] = z;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vtx(x: f32, y: f32, z: f32, color: u32) -> Vertex {
        Vertex {
            x,
            y,
            z,
            rhw: 1.0,
            color: unpack(color),
            spec: [0.0; 4],
            uv: [[0.0; 2]; 2],
        }
    }

    fn no_tex() -> [Option<TextureRef>; 2] {
        [None, None]
    }

    fn stages() -> [StageState; 2] {
        [StageState::default_for(0), StageState::default_for(1)]
    }

    fn quad_state() -> RenderState {
        RenderState {
            cull: 1,
            zenable: false,
            ..RenderState::default()
        }
    }

    fn fill(t: &mut Target, mem: &Memory, st: &RenderState, color: u32) {
        let (a, b, c, d) = (
            vtx(0.0, 0.0, 0.5, color),
            vtx(t.w as f32, 0.0, 0.5, color),
            vtx(0.0, t.h as f32, 0.5, color),
            vtx(t.w as f32, t.h as f32, 0.5, color),
        );
        draw_triangle(t, mem, st, &stages(), &no_tex(), [&a, &b, &c]);
        draw_triangle(t, mem, st, &stages(), &no_tex(), [&b, &d, &c]);
    }

    #[test]
    fn two_triangles_cover_a_rectangle_exactly() {
        let mem = Memory::new();
        let mut t = Target::new(8, 8);
        let mut st = quad_state();
        st.cull = 1;
        fill(&mut t, &mem, &st, 0xFF112233);
        assert!(
            t.color.iter().all(|&p| p == 0xFF112233),
            "every pixel covered once, none missed on the shared edge"
        );
    }

    #[test]
    fn colors_interpolate_across_a_triangle() {
        let mem = Memory::new();
        let mut t = Target::new(16, 16);
        let (a, b, c) = (
            vtx(0.0, 0.0, 0.5, 0xFFFF0000),
            vtx(16.0, 0.0, 0.5, 0xFF00FF00),
            vtx(0.0, 16.0, 0.5, 0xFF0000FF),
        );
        draw_triangle(
            &mut t,
            &mem,
            &quad_state(),
            &stages(),
            &no_tex(),
            [&a, &b, &c],
        );
        let near_a = unpack(t.color[0]);
        assert!(near_a[0] > 0.8 && near_a[1] < 0.15, "red corner is red");
        let mid = unpack(t.color[(3 * 16 + 8) as usize]);
        assert!(
            mid[0] > 0.0 && mid[1] > 0.0 && mid[2] > 0.0,
            "blended in the interior"
        );
        assert_eq!(
            t.color[(15 * 16 + 15) as usize],
            0,
            "outside the triangle untouched"
        );
    }

    #[test]
    fn backface_culling_follows_winding() {
        let mem = Memory::new();
        let tri = |ccw: bool| {
            let (a, b, c) = (
                vtx(1.0, 1.0, 0.5, 0xFFFFFFFF),
                vtx(7.0, 1.0, 0.5, 0xFFFFFFFF),
                vtx(1.0, 7.0, 0.5, 0xFFFFFFFF),
            );
            if ccw { [a, c, b] } else { [a, b, c] }
        };
        let drawn = |cull: u32, ccw: bool| {
            let mut t = Target::new(8, 8);
            let st = RenderState {
                cull,
                zenable: false,
                ..RenderState::default()
            };
            let v = tri(ccw);
            draw_triangle(
                &mut t,
                &mem,
                &st,
                &stages(),
                &no_tex(),
                [&v[0], &v[1], &v[2]],
            );
            t.color.iter().any(|&p| p != 0)
        };
        // On screen (y down), a-b-c above is clockwise. D3D's default culls counter-clockwise (3).
        assert!(drawn(3, false), "clockwise triangles survive CULL_CCW");
        assert!(!drawn(3, true), "counter-clockwise ones are culled");
        assert!(
            drawn(2, true) && !drawn(2, false),
            "CULL_CW is the mirror image"
        );
        assert!(drawn(1, true) && drawn(1, false), "CULL_NONE draws both");
    }

    #[test]
    fn depth_test_keeps_the_nearer_surface() {
        let mem = Memory::new();
        let mut t = Target::new(8, 8);
        let st = RenderState {
            cull: 1,
            ..RenderState::default()
        };
        let tri = |z: f32, color: u32| {
            let (a, b, c) = (
                vtx(0.0, 0.0, z, color),
                vtx(16.0, 0.0, z, color),
                vtx(0.0, 16.0, z, color),
            );
            [a, b, c]
        };
        let far = tri(0.8, 0xFF0000FF);
        let near = tri(0.2, 0xFFFF0000);
        draw_triangle(
            &mut t,
            &mem,
            &st,
            &stages(),
            &no_tex(),
            [&near[0], &near[1], &near[2]],
        );
        draw_triangle(
            &mut t,
            &mem,
            &st,
            &stages(),
            &no_tex(),
            [&far[0], &far[1], &far[2]],
        );
        assert_eq!(
            t.color[9], 0xFFFF0000,
            "the farther triangle did not overwrite the nearer one"
        );
        assert!((t.depth[9] - 0.2).abs() < 1e-6);
        let mut st2 = st.clone();
        st2.zwrite = false;
        let mut t2 = Target::new(8, 8);
        draw_triangle(
            &mut t2,
            &mem,
            &st2,
            &stages(),
            &no_tex(),
            [&near[0], &near[1], &near[2]],
        );
        assert_eq!(
            t2.depth[9], 1.0,
            "ZWRITEENABLE false leaves the depth buffer alone"
        );
    }

    #[test]
    fn alpha_blending_and_alpha_test() {
        let mem = Memory::new();
        let mut t = Target::new(4, 4);
        t.clear(Some(0xFF0000FF), None); // opaque blue
        let st = RenderState {
            cull: 1,
            zenable: false,
            alpha_blend: true,
            src_blend: 5,
            dst_blend: 6,
            ..RenderState::default()
        };
        fill(&mut t, &mem, &st, 0x80FF0000); // 50% red over blue
        let p = unpack(t.color[5]);
        assert!(
            (p[0] - 0.5).abs() < 0.02 && (p[2] - 0.5).abs() < 0.02,
            "half red, half blue: {p:?}"
        );

        let mut t = Target::new(4, 4);
        let st = RenderState {
            cull: 1,
            zenable: false,
            alpha_test: true,
            alpha_func: CMP_GREATEREQUAL,
            alpha_ref: 0x80,
            ..RenderState::default()
        };
        fill(&mut t, &mem, &st, 0x40FFFFFF);
        assert!(
            t.color.iter().all(|&p| p == 0),
            "alpha 0x40 fails ALPHAREF 0x80"
        );
        fill(&mut t, &mem, &st, 0xC0FFFFFF);
        assert!(t.color.iter().all(|&p| p != 0), "alpha 0xC0 passes");
    }

    fn texture_in_memory(mem: &mut Memory, format: u32, w: u32, h: u32, data: &[u8]) -> TextureRef {
        mem.map(0x10000, 0x10000);
        mem.write_bytes(0x10000, data).unwrap();
        let (pitch, _) = surface_size(format, w, h);
        TextureRef {
            format,
            levels: vec![Level {
                ptr: 0x10000,
                w,
                h,
                pitch,
            }],
        }
    }

    fn textured_quad(mem: &Memory, tex: &TextureRef, st: &StageState, size: u32) -> Vec<u32> {
        let mut t = Target::new(size, size);
        let s = size as f32;
        let mk = |x: f32, y: f32, u: f32, v: f32| {
            let mut vt = vtx(x, y, 0.5, 0xFFFFFFFF);
            vt.uv[0] = [u, v];
            vt
        };
        let (a, b, c, d) = (
            mk(0.0, 0.0, 0.0, 0.0),
            mk(s, 0.0, 1.0, 0.0),
            mk(0.0, s, 0.0, 1.0),
            mk(s, s, 1.0, 1.0),
        );
        let stages = [*st, StageState::default_for(1)];
        let texs = [Some(tex.clone()), None];
        let rs = quad_state();
        draw_triangle(&mut t, mem, &rs, &stages, &texs, [&a, &b, &c]);
        draw_triangle(&mut t, mem, &rs, &stages, &texs, [&b, &d, &c]);
        t.color
    }

    #[test]
    fn point_sampled_texture_maps_texels_to_pixels() {
        let mut mem = Memory::new();
        // 2x2 A8R8G8B8: red, green / blue, white
        let px: Vec<u8> = [0xFFFF0000u32, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF]
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let tex = texture_in_memory(&mut mem, FMT_A8R8G8B8, 2, 2, &px);
        let out = textured_quad(&mem, &tex, &StageState::default_for(0), 4);
        assert_eq!(
            (out[0], out[3], out[12], out[15]),
            (0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF),
            "each quadrant shows its texel"
        );
    }

    #[test]
    fn bilinear_filtering_blends_neighbouring_texels() {
        let mut mem = Memory::new();
        let px: Vec<u8> = [0xFF000000u32, 0xFFFFFFFF]
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let tex = texture_in_memory(&mut mem, FMT_A8R8G8B8, 2, 1, &px);
        let st = StageState {
            mag: 2,
            addr_u: 3,
            addr_v: 3,
            ..StageState::default_for(0)
        };
        let out = textured_quad(&mem, &tex, &st, 8);
        let mid = unpack(out[3]);
        assert!(
            mid[0] > 0.1 && mid[0] < 0.9,
            "a pixel between the two texels is grey: {mid:?}"
        );
        assert!(
            unpack(out[0])[0] < 0.1 && unpack(out[7])[0] > 0.9,
            "the ends stay black and white"
        );
    }

    #[test]
    fn address_modes_wrap_mirror_and_clamp() {
        assert!(
            (wrap_coord(1.25, 4, 1) - 1.0).abs() < 1e-5,
            "wrap: 1.25 -> 0.25 * 4"
        );
        assert!(
            (wrap_coord(-0.25, 4, 1) - 3.0).abs() < 1e-5,
            "wrap handles negatives"
        );
        assert!(
            (wrap_coord(1.25, 4, 2) - 3.0).abs() < 1e-5,
            "mirror: 1.25 -> 0.75 * 4"
        );
        assert_eq!(wrap_coord(5.0, 4, 3), 4.0, "clamp");
        assert_eq!(wrap_coord(-5.0, 4, 3), 0.0);
    }

    #[test]
    fn packed_pixel_formats_decode() {
        let mut mem = Memory::new();
        mem.map(0x10000, 0x1000);
        let lvl = Level {
            ptr: 0x10000,
            w: 1,
            h: 1,
            pitch: 4,
        };
        mem.write(0x10000, 2, 0xF800).unwrap(); // R5G6B5 pure red
        assert_eq!(pack(texel(&mem, FMT_R5G6B5, &lvl, 0, 0)), 0xFFFF0000);
        mem.write(0x10000, 2, 0x8000 | (31 << 5)).unwrap(); // A1R5G5B5 opaque green
        assert_eq!(pack(texel(&mem, FMT_A1R5G5B5, &lvl, 0, 0)), 0xFF00FF00);
        mem.write(0x10000, 2, 0x8421).unwrap();
        mem.write(0x10000, 2, 0x80F0).unwrap(); // A4R4G4B4: a=8 r=0 g=F b=0
        let c = texel(&mem, FMT_A4R4G4B4, &lvl, 0, 0);
        assert!((c[3] - 8.0 / 15.0).abs() < 1e-5 && c[1] == 1.0 && c[0] == 0.0);
        mem.write(0x10000, 1, 0x80).unwrap();
        let l = texel(&mem, FMT_L8, &lvl, 0, 0);
        assert!((l[0] - 128.0 / 255.0).abs() < 1e-5 && l[3] == 1.0);
        let a = texel(&mem, FMT_A8, &lvl, 0, 0);
        assert_eq!(a[0], 1.0);
        assert!((a[3] - 128.0 / 255.0).abs() < 1e-5);
    }

    #[test]
    fn dxt1_block_decodes_its_four_colors() {
        let mut mem = Memory::new();
        mem.map(0x10000, 0x1000);
        // c0 = pure red (0xF800), c1 = pure blue (0x001F); indices 0,1,2,3 across the first row
        let mut block = Vec::new();
        block.extend_from_slice(&0xF800u16.to_le_bytes());
        block.extend_from_slice(&0x001Fu16.to_le_bytes());
        block.extend_from_slice(&[0b1110_0100, 0, 0, 0]);
        mem.write_bytes(0x10000, &block).unwrap();
        let lvl = Level {
            ptr: 0x10000,
            w: 4,
            h: 4,
            pitch: 8,
        };
        assert_eq!(
            pack(texel(&mem, FMT_DXT1, &lvl, 0, 0)),
            0xFFFF0000,
            "index 0 = c0"
        );
        assert_eq!(
            pack(texel(&mem, FMT_DXT1, &lvl, 1, 0)),
            0xFF0000FF,
            "index 1 = c1"
        );
        let two_thirds_red = texel(&mem, FMT_DXT1, &lvl, 2, 0);
        assert!(
            (two_thirds_red[0] - 2.0 / 3.0).abs() < 0.02
                && (two_thirds_red[2] - 1.0 / 3.0).abs() < 0.02,
            "index 2 = 2/3 c0 + 1/3 c1"
        );
    }

    #[test]
    fn dxt1_with_c0_not_greater_than_c1_has_a_transparent_texel() {
        let mut mem = Memory::new();
        mem.map(0x10000, 0x1000);
        let mut block = Vec::new();
        block.extend_from_slice(&0x001Fu16.to_le_bytes()); // c0 <= c1: three-colour + transparent mode
        block.extend_from_slice(&0xF800u16.to_le_bytes());
        block.extend_from_slice(&[0b1100_0000, 0, 0, 0]); // pixel 3 = index 3
        mem.write_bytes(0x10000, &block).unwrap();
        let lvl = Level {
            ptr: 0x10000,
            w: 4,
            h: 4,
            pitch: 8,
        };
        assert_eq!(
            texel(&mem, FMT_DXT1, &lvl, 3, 0)[3],
            0.0,
            "index 3 is fully transparent"
        );
        assert_eq!(texel(&mem, FMT_DXT1, &lvl, 0, 0)[3], 1.0);
    }

    #[test]
    fn dxt3_and_dxt5_alpha() {
        let mut mem = Memory::new();
        mem.map(0x10000, 0x1000);
        // DXT3: 8 bytes of 4-bit alpha, then a colour block (all c0 = white)
        let mut b3 = vec![0u8; 16];
        b3[0] = 0xF0; // pixel 0 alpha 0, pixel 1 alpha 15
        b3[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes());
        b3[10..12].copy_from_slice(&0xFFFFu16.to_le_bytes());
        mem.write_bytes(0x10000, &b3).unwrap();
        let lvl = Level {
            ptr: 0x10000,
            w: 4,
            h: 4,
            pitch: 16,
        };
        assert_eq!(texel(&mem, FMT_DXT3, &lvl, 0, 0)[3], 0.0);
        assert_eq!(texel(&mem, FMT_DXT3, &lvl, 1, 0)[3], 1.0);

        // DXT5: a0 = 255, a1 = 0 (8-alpha mode); index 0 -> 255, index 1 -> 0, index 2 -> 6/7 * 255 + ...
        let mut b5 = vec![0u8; 16];
        b5[0] = 255;
        b5[1] = 0;
        b5[2] = 0b0000_1000; // pixel 0 -> idx 0, pixel 1 -> idx 1, pixel 2 -> idx 0
        b5[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes());
        b5[10..12].copy_from_slice(&0xFFFFu16.to_le_bytes());
        mem.write_bytes(0x10000, &b5).unwrap();
        assert_eq!(texel(&mem, FMT_DXT5, &lvl, 0, 0)[3], 1.0);
        assert_eq!(texel(&mem, FMT_DXT5, &lvl, 1, 0)[3], 0.0);
    }

    #[test]
    fn surface_sizes() {
        assert_eq!(surface_size(FMT_A8R8G8B8, 16, 8), (64, 512));
        assert_eq!(surface_size(FMT_R5G6B5, 16, 8), (32, 256));
        assert_eq!(
            surface_size(FMT_DXT1, 16, 16),
            (32, 128),
            "4x4 blocks of 8 bytes"
        );
        assert_eq!(surface_size(FMT_DXT5, 16, 16), (64, 256));
        assert_eq!(
            surface_size(FMT_DXT1, 1, 1),
            (8, 8),
            "a 1x1 mip level still occupies a whole block"
        );
    }

    #[test]
    fn texture_stage_combiners() {
        let mem = Memory::new();
        let vtx_color = Vertex {
            color: [0.5, 0.5, 0.5, 1.0],
            ..Vertex::default()
        };
        let st = RenderState::default();
        let run = |s0: StageState| {
            shade(
                &mem,
                &st,
                &[s0, StageState::default_for(1)],
                &[None, None],
                &vtx_color,
            )
        };
        // no texture bound and the stage wants one: it is disabled, so the vertex colour passes through
        assert_eq!(run(StageState::default_for(0)), [0.5, 0.5, 0.5, 1.0]);
        let tfactor_stage = StageState {
            color_op: 4,
            color_arg1: 0,
            color_arg2: 3,
            alpha_op: 2,
            alpha_arg1: 0,
            ..StageState::default_for(0)
        }; // diffuse * tfactor
        assert_eq!(
            run(tfactor_stage)[0],
            0.5,
            "a stage that does not need a texture still runs (tfactor is white)"
        );
        let add = StageState {
            color_op: 7,
            color_arg1: 0,
            color_arg2: 0,
            alpha_arg1: 0,
            ..StageState::default_for(0)
        }; // diffuse + diffuse
        assert_eq!(run(add)[0], 1.0);
        let x2 = StageState {
            color_op: 5,
            color_arg1: 0,
            color_arg2: 0,
            alpha_arg1: 0,
            ..StageState::default_for(0)
        }; // diffuse * diffuse * 2
        assert_eq!(run(x2)[0], 0.5);
        let comp = StageState {
            color_op: 2,
            color_arg1: 0x10,
            alpha_arg1: 0,
            ..StageState::default_for(0)
        }; // 1 - diffuse
        assert_eq!(run(comp)[0], 0.5);
        let disabled = StageState {
            color_op: 1,
            ..StageState::default_for(0)
        };
        assert_eq!(
            run(disabled),
            [0.5, 0.5, 0.5, 1.0],
            "DISABLE passes the vertex colour through"
        );
    }
}
