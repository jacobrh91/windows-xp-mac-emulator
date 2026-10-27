//! The vertex pipeline and drawing: vertex formats, transform and lighting, near-plane
//! clipping, and `DrawPrimitive`/`DrawIndexedPrimitive` onto the rasterizer or the GPU.

use super::*;

#[derive(Clone, Debug)]
pub(super) struct FvfLayout {
    pub(super) xyzrhw: bool,
    pub(super) has_xyz: bool,
    pub(super) weights: u32,
    pub(super) normal: bool,
    pub(super) psize: bool,
    pub(super) diffuse: bool,
    pub(super) specular: bool,
    pub(super) tex_count: u32,
    /// Floats per coordinate set.
    pub(super) tex_floats: [u32; 8],
    pub(super) stride: u32,
}

pub(super) fn parse_fvf(fvf: u32) -> FvfLayout {
    let pos = fvf & 0xE;
    let xyzrhw = pos == 0x4;
    let weights = match pos {
        0x6 => 1,
        0x8 => 2,
        0xA => 3,
        0xC => 4,
        0xE => 5,
        _ => 0,
    };
    let mut l = FvfLayout {
        xyzrhw,
        has_xyz: pos != 0,
        weights,
        normal: fvf & 0x10 != 0,
        psize: fvf & 0x20 != 0,
        diffuse: fvf & 0x40 != 0,
        specular: fvf & 0x80 != 0,
        tex_count: ((fvf >> 8) & 0xF).min(8),
        tex_floats: [2; 8],
        stride: 0,
    };
    let mut stride = if xyzrhw {
        16
    } else if pos != 0 {
        12
    } else {
        0
    };
    stride += 4 * weights;
    stride += if l.normal { 12 } else { 0 }
        + if l.psize { 4 } else { 0 }
        + if l.diffuse { 4 } else { 0 }
        + if l.specular { 4 } else { 0 };
    for i in 0..l.tex_count as usize {
        l.tex_floats[i] = match (fvf >> (16 + 2 * i)) & 3 {
            0 => 2,
            1 => 3,
            2 => 4,
            _ => 1,
        };
        stride += 4 * l.tex_floats[i];
    }
    l.stride = stride;
    l
}

/// A vertex before the perspective divide (or already in screen space).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PipeVertex {
    pub(super) clip: [f32; 4],
    pub(super) color: [f32; 4],
    pub(super) spec: [f32; 4],
    pub(super) uv: [[f32; 2]; 2],
    /// Already screen-space (XYZRHW): x, y, z, rhw are in `clip`.
    pub(super) screen: bool,
}

pub(super) fn mat_mul_point(m: &[f32; 16], p: [f32; 4]) -> [f32; 4] {
    let mut o = [0.0; 4];
    for c in 0..4 {
        o[c] = p[0] * m[c] + p[1] * m[4 + c] + p[2] * m[8 + c] + p[3] * m[12 + c];
    }
    o
}

pub(super) fn mat_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut o = [0.0; 16];
    for r in 0..4 {
        for c in 0..4 {
            o[r * 4 + c] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum();
        }
    }
    o
}

pub(super) fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l > 1e-12 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        v
    }
}

impl Device {
    pub(super) fn rs_bool(&self, s: usize) -> bool {
        self.rs[s] != 0
    }

    /// The enabled lights, decoded from their D3DLIGHT8 bytes (once per draw, not per vertex).
    pub(super) fn light_params(&self) -> Vec<LightParams> {
        self.lights
            .values()
            .filter(|l| l.enabled)
            .map(|light| {
                let f = |off: usize| {
                    f32::from_bits(u32::from_le_bytes([
                        light.raw[off],
                        light.raw[off + 1],
                        light.raw[off + 2],
                        light.raw[off + 3],
                    ]))
                };
                let d = normalize3([f(64), f(68), f(72)]);
                LightParams {
                    directional: u32::from_le_bytes([
                        light.raw[0],
                        light.raw[1],
                        light.raw[2],
                        light.raw[3],
                    ]) == 3,
                    diffuse: [f(4), f(8), f(12)],
                    ambient: [f(36), f(40), f(44)],
                    pos: [f(52), f(56), f(60)],
                    to_light: [-d[0], -d[1], -d[2]],
                    range: f(76),
                    atten: [f(84), f(88), f(92)],
                }
            })
            .collect()
    }

    pub(super) fn lit_color(
        &self,
        lights: &[LightParams],
        world_pos: [f32; 3],
        world_normal: [f32; 3],
        vertex_diffuse: Option<[f32; 4]>,
    ) -> [f32; 4] {
        let m = &self.material;
        let color_vertex = self.rs_bool(141);
        let src = |state: usize, vertex: Option<[f32; 4]>, mat: [f32; 4]| -> [f32; 4] {
            match (color_vertex, self.rs[state], vertex) {
                (true, 1, Some(c)) => c,
                _ => mat,
            }
        };
        let diffuse_mat = src(145, vertex_diffuse, [m[0], m[1], m[2], m[3]]);
        let ambient_mat = src(147, vertex_diffuse, [m[4], m[5], m[6], m[7]]);
        let emissive_mat = src(148, vertex_diffuse, [m[12], m[13], m[14], m[15]]);
        let global_ambient = unpack(self.rs[139]);
        let mut rgb = [0.0f32; 3];
        for c in 0..3 {
            rgb[c] = emissive_mat[c] + global_ambient[c] * ambient_mat[c];
        }
        let n = normalize3(world_normal);
        for light in lights {
            let (light_diffuse, light_ambient, pos) = (light.diffuse, light.ambient, light.pos);
            let (l, atten) = if light.directional {
                (light.to_light, 1.0)
            } else {
                let v = [
                    pos[0] - world_pos[0],
                    pos[1] - world_pos[1],
                    pos[2] - world_pos[2],
                ];
                let dist = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                if light.range > 0.0 && dist > light.range {
                    continue;
                }
                let [a0, a1, a2] = light.atten;
                let denom = a0 + a1 * dist + a2 * dist * dist;
                (normalize3(v), if denom > 1e-9 { 1.0 / denom } else { 1.0 })
            };
            let ndl = (n[0] * l[0] + n[1] * l[1] + n[2] * l[2]).max(0.0);
            for c in 0..3 {
                rgb[c] += atten
                    * (light_ambient[c] * ambient_mat[c] + ndl * light_diffuse[c] * diffuse_mat[c]);
            }
        }
        [
            rgb[0].clamp(0.0, 1.0),
            rgb[1].clamp(0.0, 1.0),
            rgb[2].clamp(0.0, 1.0),
            diffuse_mat[3],
        ]
    }

    pub(super) fn process(
        &self,
        e: &Emu,
        l: &FvfLayout,
        setup: &DrawSetup,
        addr: u32,
    ) -> Res<PipeVertex> {
        let mut o = addr;
        let mut v = PipeVertex {
            color: [1.0; 4],
            ..PipeVertex::default()
        };
        let mut pos = [0.0f32; 3];
        if l.xyzrhw {
            v.clip = [
                read_f32(e, o)?,
                read_f32(e, o + 4)?,
                read_f32(e, o + 8)?,
                read_f32(e, o + 12)?,
            ];
            v.screen = true;
            o += 16;
        } else if l.has_xyz {
            pos = [read_f32(e, o)?, read_f32(e, o + 4)?, read_f32(e, o + 8)?];
            o += 12;
        }
        o += 4 * l.weights;
        let mut normal = [0.0f32; 3];
        if l.normal {
            normal = [read_f32(e, o)?, read_f32(e, o + 4)?, read_f32(e, o + 8)?];
            o += 12;
        }
        if l.psize {
            o += 4;
        }
        let mut diffuse = None;
        if l.diffuse {
            diffuse = Some(unpack(e.mem.read(o, 4)?));
            o += 4;
        }
        if l.specular {
            v.spec = unpack(e.mem.read(o, 4)?);
            v.spec[3] = 0.0;
            o += 4;
        }
        for i in 0..l.tex_count as usize {
            if i < 2 {
                v.uv[i] = [
                    read_f32(e, o)?,
                    if l.tex_floats[i] >= 2 {
                        read_f32(e, o + 4)?
                    } else {
                        0.0
                    },
                ];
                // Texture coordinate transform (2D only).
                let flags = self.ts[i][24] & 0xFF;
                if flags != 0 {
                    let t =
                        mat_mul_point(&self.tex_transform[i], [v.uv[i][0], v.uv[i][1], 0.0, 1.0]);
                    v.uv[i] = if self.ts[i][24] & 0x100 != 0 && t[2].abs() > 1e-9 {
                        [t[0] / t[2], t[1] / t[2]]
                    } else {
                        [t[0], t[1]]
                    };
                }
            }
            o += 4 * l.tex_floats[i];
        }
        if v.screen {
            v.color = diffuse.unwrap_or([1.0; 4]);
            return Ok(v);
        }
        // 3D vertex: world, view, projection.
        let wp = mat_mul_point(&self.world, [pos[0], pos[1], pos[2], 1.0]);
        v.clip = mat_mul_point(&setup.wvp, [pos[0], pos[1], pos[2], 1.0]);
        v.color = if self.rs_bool(137) && l.normal {
            // Normals use the world matrix's rotation part.
            let wn = [
                normal[0] * self.world[0] + normal[1] * self.world[4] + normal[2] * self.world[8],
                normal[0] * self.world[1] + normal[1] * self.world[5] + normal[2] * self.world[9],
                normal[0] * self.world[2] + normal[1] * self.world[6] + normal[2] * self.world[10],
            ];
            self.lit_color(&setup.lights, [wp[0], wp[1], wp[2]], wn, diffuse)
        } else {
            diffuse.unwrap_or([1.0; 4])
        };
        Ok(v)
    }

    pub(super) fn to_screen(&self, v: &PipeVertex) -> Vertex {
        let mut out = Vertex {
            color: v.color,
            spec: v.spec,
            uv: v.uv,
            ..Vertex::default()
        };
        if v.screen {
            (out.x, out.y, out.z, out.rhw) = (v.clip[0], v.clip[1], v.clip[2], v.clip[3]);
        } else {
            let w = v.clip[3];
            let inv = if w.abs() > 1e-9 { 1.0 / w } else { 1.0 };
            let (vx, vy, vw, vh) = (
                self.viewport[0] as f32,
                self.viewport[1] as f32,
                self.viewport[2] as f32,
                self.viewport[3] as f32,
            );
            let (min_z, max_z) = (
                f32::from_bits(self.viewport[4]),
                f32::from_bits(self.viewport[5]),
            );
            out.x = vx + (v.clip[0] * inv * 0.5 + 0.5) * vw;
            out.y = vy + (0.5 - v.clip[1] * inv * 0.5) * vh;
            out.z = min_z + v.clip[2] * inv * (max_z - min_z);
            out.rhw = inv;
        }
        out
    }
}

/// A light, decoded for the vertex loop.
pub(super) struct LightParams {
    pub(super) directional: bool,
    pub(super) diffuse: [f32; 3],
    pub(super) ambient: [f32; 3],
    pub(super) pos: [f32; 3],
    /// For directional lights: the unit vector toward the light.
    pub(super) to_light: [f32; 3],
    pub(super) range: f32,
    pub(super) atten: [f32; 3],
}

/// What every vertex of one draw call shares.
pub(super) struct DrawSetup {
    pub(super) wvp: [f32; 16],
    pub(super) lights: Vec<LightParams>,
}

pub(super) fn lerp_pv(a: &PipeVertex, b: &PipeVertex, t: f32) -> PipeVertex {
    let l = |x: f32, y: f32| x + (y - x) * t;
    let mut o = *a;
    for i in 0..4 {
        o.clip[i] = l(a.clip[i], b.clip[i]);
        o.color[i] = l(a.color[i], b.color[i]);
        o.spec[i] = l(a.spec[i], b.spec[i]);
    }
    for s in 0..2 {
        for k in 0..2 {
            o.uv[s][k] = l(a.uv[s][k], b.uv[s][k]);
        }
    }
    o
}

/// Clip a triangle against the near plane (z >= 0 and w > 0) in clip space: the polygon
/// (at most four vertices) and how many vertices it has.
pub(super) fn clip_near(tri: [PipeVertex; 3]) -> ([PipeVertex; 4], usize) {
    let inside = |v: &PipeVertex| v.clip[2] >= 0.0 && v.clip[3] > 1e-6;
    let mut out = [PipeVertex::default(); 4];
    let mut len = 0;
    for i in 0..3 {
        let (cur, next) = (&tri[i], &tri[(i + 1) % 3]);
        let (ci, ni) = (inside(cur), inside(next));
        if ci {
            out[len] = *cur;
            len += 1;
        }
        if ci != ni {
            // Intersect with z = 0 (or w = epsilon if z is fine but w is not).
            let t = if (cur.clip[2] - next.clip[2]).abs() > 1e-12 {
                cur.clip[2] / (cur.clip[2] - next.clip[2])
            } else {
                0.5
            };
            out[len] = lerp_pv(cur, next, t.clamp(0.0, 1.0));
            len += 1;
        }
    }
    (out, len)
}

pub(super) enum DrawCall {
    Stream {
        ptype: u32,
        start: u32,
        count: u32,
        indexed: Option<(u32, u32, u32)>,
    },
    UserData {
        ptype: u32,
        count: u32,
        vertices: u32,
        stride: u32,
        indices: Option<(u32, u32)>,
    },
}

pub(super) fn index_count(ptype: u32, prims: u32) -> u32 {
    match ptype {
        1 => prims,
        2 => prims * 2,
        3 => prims + 1,
        4 => prims * 3,
        _ => prims + 2, // strip, fan
    }
}

/// How many triangles `n` vertex slots make for a primitive type (lines and points are
/// handled separately).
pub(super) fn triangle_count(ptype: u32, n: u32) -> u32 {
    match ptype {
        4 => n / 3,
        5 | 6 => n.saturating_sub(2),
        _ => 0,
    }
}

/// The vertex slots of triangle `i`.
pub(super) fn triangle_slot(ptype: u32, i: u32) -> [u32; 3] {
    match ptype {
        4 => [3 * i, 3 * i + 1, 3 * i + 2],
        // alternate winding so every triangle faces the same way
        5 if i % 2 == 0 => [i, i + 1, i + 2],
        5 => [i + 1, i, i + 2],
        _ => [0, i + 1, i + 2],
    }
}

/// XP_D3D_PROBE=x,y: the pixel whose draws are described (debugging).
pub(super) fn probe_point() -> Option<(f32, f32)> {
    static P: std::sync::OnceLock<Option<(f32, f32)>> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        let v = std::env::var("XP_D3D_PROBE").ok()?;
        let (x, y) = v.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    })
}

/// Does the screen-space triangle `t` cover pixel centre (px, py)?
pub(super) fn covers(t: &[Vertex], px: f32, py: f32) -> bool {
    let e = |a: &Vertex, b: &Vertex| (b.x - a.x) * (py - a.y) - (b.y - a.y) * (px - a.x);
    let (w0, w1, w2) = (e(&t[1], &t[2]), e(&t[2], &t[0]), e(&t[0], &t[1]));
    (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0) || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0)
}

pub(super) fn draw(e: &mut Emu, call: &DrawCall) -> Res<u32> {
    let Some(()) = with_device(e, |e, d| draw_inner(e, d, call))? else {
        return Ok(D3DERR_INVALIDCALL);
    };
    Ok(S_OK)
}

pub(super) fn draw_inner(e: &mut Emu, d: &mut Device, call: &DrawCall) -> Res<()> {
    if d.shader & 0x8000_0000 != 0 {
        if e.win.trace {
            eprintln!("[d3d8] draw skipped: programmable vertex shaders are not supported");
        }
        return Ok(());
    }
    let layout = parse_fvf(d.shader);
    if layout.stride == 0 {
        return Ok(());
    }
    let (ptype, prims) = match call {
        DrawCall::Stream { ptype, count, .. } | DrawCall::UserData { ptype, count, .. } => {
            (*ptype, *count)
        }
    };
    let n = index_count(ptype, prims);
    // Where do vertex data and indices live?
    let (vbase, stride, index_source): (u32, u32, Option<(u32, bool, u32)>) = match call {
        DrawCall::Stream { start, indexed, .. } => {
            let (vb, vstride) = d.stream;
            let Some(Obj::VertexBuffer(buf)) = e.win.d3d.objects.get(&vb) else {
                return Ok(());
            };
            let stride = if vstride != 0 { vstride } else { layout.stride };
            match indexed {
                None => (buf.ptr + start * stride, stride, None),
                Some((start_index, _min, _)) => {
                    let (ib, base) = d.indices;
                    let Some(Obj::IndexBuffer(ibuf)) = e.win.d3d.objects.get(&ib) else {
                        return Ok(());
                    };
                    let wide = ibuf.kind == 102;
                    let isz = if wide { 4 } else { 2 };
                    (
                        buf.ptr + base * stride,
                        stride,
                        Some((ibuf.ptr + start_index * isz, wide, 0)),
                    )
                }
            }
        }
        DrawCall::UserData {
            vertices,
            stride,
            indices,
            ..
        } => (
            *vertices,
            *stride,
            indices.map(|(p, fmt)| (p, fmt == 102, 0)),
        ),
    };
    let slot_to_vertex = |e: &Emu, slot: u32| -> Res<u32> {
        match index_source {
            None => Ok(slot),
            Some((ptr, wide, _)) => Ok(if wide {
                e.mem.read(ptr + 4 * slot, 4)?
            } else {
                e.mem.read(ptr + 2 * slot, 2)?
            }),
        }
    };
    let setup = DrawSetup {
        wvp: mat_mul(&mat_mul(&d.world, &d.view), &d.proj),
        lights: if d.rs_bool(137) && layout.normal {
            d.light_params()
        } else {
            Vec::new()
        },
    };
    // Indexed meshes reuse vertices; each is transformed once per draw. The cache and the
    // GPU vertex list live in the device between draws so drawing allocates nothing; a cache
    // entry is valid when its stamp is this draw's.
    const CACHE_LIMIT: usize = 1 << 16;
    let mut scratch = std::mem::take(&mut d.scratch);
    scratch.stamp = scratch.stamp.wrapping_add(1);
    if scratch.stamp == 0 {
        scratch.cache.iter_mut().for_each(|c| c.0 = 0);
        scratch.stamp = 1;
    }
    let stamp = scratch.stamp;
    let cache = &mut scratch.cache;
    // The cache entry holding the vertex at `slot`, transforming it if this draw has not yet.
    // Vertices past the cache's limit go through one of three spare entries (`corner`).
    let fetch = |cache: &mut Vec<(u32, Projected, PipeVertex, Vertex)>,
                 e: &Emu,
                 d: &Device,
                 slot: u32,
                 corner: usize|
     -> Res<usize> {
        let vi = slot_to_vertex(e, slot)?;
        let key = (vi as usize).min(CACHE_LIMIT)
            + if vi as usize >= CACHE_LIMIT {
                corner
            } else {
                0
            };
        if key < CACHE_LIMIT
            && let Some(c) = cache.get(key)
            && c.0 == stamp
        {
            return Ok(key);
        }
        let v = d.process(e, &layout, &setup, vbase + vi * stride)?;
        let kind = if v.screen {
            Projected::Screen
        } else if v.clip[2] >= 0.0 && v.clip[3] > 1e-6 {
            Projected::Front
        } else {
            Projected::Behind
        };
        let scr = if kind == Projected::Behind {
            Vertex::default()
        } else {
            d.to_screen(&v)
        };
        if cache.len() <= key {
            cache.resize((key + 1).next_power_of_two(), Default::default());
        }
        cache[key] = (stamp, kind, v, scr);
        Ok(key)
    };

    // Render state for the rasterizer.
    let rs = RenderState {
        zenable: d.rs_bool(7),
        zwrite: d.rs_bool(14),
        zfunc: d.rs[23].clamp(1, 8),
        alpha_blend: d.rs_bool(27),
        src_blend: d.rs[19],
        dst_blend: d.rs[20],
        alpha_test: d.rs_bool(15),
        alpha_ref: d.rs[24],
        alpha_func: d.rs[25].clamp(1, 8),
        cull: d.rs[22].clamp(1, 3),
        texture_factor: d.rs[60],
        color_write: d.rs[168],
        specular: d.rs_bool(29),
        scissor: [
            d.viewport[0],
            d.viewport[1],
            d.viewport[0].saturating_add(d.viewport[2]),
            d.viewport[1].saturating_add(d.viewport[3]),
        ],
    };
    let stage = |s: usize| -> StageState {
        let t = &d.ts[s];
        StageState {
            color_op: t[1],
            color_arg1: t[2],
            color_arg2: t[3],
            alpha_op: t[4],
            alpha_arg1: t[5],
            alpha_arg2: t[6],
            addr_u: t[13],
            addr_v: t[14],
            mag: t[16],
            min: t[17],
            texcoord_index: t[11] & 0xFFFF,
        }
    };
    let stages = [stage(0), stage(1)];
    let mut texs: [Option<TextureRef>; 2] = [None, None];
    for (s, slot) in texs.iter_mut().enumerate() {
        if let Some(Obj::Texture(t)) = e.win.d3d.objects.get(&d.textures[s]) {
            *slot = Some(TextureRef {
                format: t.format,
                levels: t.levels.clone(),
            });
        }
    }

    // Textures changed by the guest since they were uploaded.
    if let Some(g) = d.gpu.as_mut() {
        for key in e.win.d3d.dirty.drain() {
            g.invalidate_texture(key);
        }
    }
    let use_gpu = d.gpu.is_some() && d.rt.is_none();
    let gpu_verts = &mut scratch.verts;
    gpu_verts.clear();

    // Rasterize into a target taken out of the device, so vertex fetching can borrow it.
    let mut rt = d.rt.take();
    let mut main = std::mem::replace(&mut d.target, Target::new(0, 0));
    let r = (|| -> Res<()> {
        let target = match rt.as_mut() {
            Some((_, t)) => t,
            None => &mut main,
        };
        let dev_view: &Device = d;
        if ptype >= 4 {
            for t in 0..triangle_count(ptype, n) {
                let tri = triangle_slot(ptype, t);
                let k = [
                    fetch(cache, e, dev_view, tri[0], 0)?,
                    fetch(cache, e, dev_view, tri[1], 1)?,
                    fetch(cache, e, dev_view, tri[2], 2)?,
                ];
                let kinds = k.map(|i| cache[i].1);
                let mut scr = [Vertex::default(); 4];
                let len = if kinds.iter().all(|&p| p == Projected::Screen)
                    || kinds.iter().all(|&p| p == Projected::Front)
                {
                    // Nothing to clip: the screen positions are already worked out.
                    for (o, &i) in scr.iter_mut().zip(&k) {
                        *o = cache[i].3;
                    }
                    3
                } else {
                    let (poly, len) = clip_near(k.map(|i| cache[i].2));
                    for (o, p) in scr.iter_mut().zip(&poly[..len]) {
                        *o = dev_view.to_screen(p);
                    }
                    len
                };
                if len < 3 {
                    continue;
                }
                for i in 1..len - 1 {
                    if use_gpu {
                        gpu_verts.extend_from_slice(&[scr[0], scr[i], scr[i + 1]]);
                    } else {
                        draw_triangle(
                            target,
                            &e.mem,
                            &rs,
                            &stages,
                            &texs,
                            [&scr[0], &scr[i], &scr[i + 1]],
                        );
                    }
                }
            }
        } else if ptype == 2 || ptype == 3 {
            let segments: Vec<(u32, u32)> = if ptype == 2 {
                (0..n / 2).map(|i| (2 * i, 2 * i + 1)).collect()
            } else {
                (0..n.saturating_sub(1)).map(|i| (i, i + 1)).collect()
            };
            for (a_slot, b_slot) in segments {
                let (ka, kb) = (
                    fetch(cache, e, dev_view, a_slot, 0)?,
                    fetch(cache, e, dev_view, b_slot, 1)?,
                );
                let (a, b) = (cache[ka].2, cache[kb].2);
                if a.clip[3] <= 1e-6 && !a.screen || b.clip[3] <= 1e-6 && !b.screen {
                    continue;
                }
                let (sa, sb) = (dev_view.to_screen(&a), dev_view.to_screen(&b));
                if use_gpu {
                    thin_quad(gpu_verts, &sa, &sb);
                } else {
                    draw_line(target, &sa, &sb, &rs);
                }
            }
        } else {
            for slot in 0..n {
                let k = fetch(cache, e, dev_view, slot, 0)?;
                let v = cache[k].2;
                let s = dev_view.to_screen(&v);
                if use_gpu {
                    thin_quad(gpu_verts, &s, &s);
                } else if s.x >= 0.0
                    && s.y >= 0.0
                    && (s.x as u32) < target.w
                    && (s.y as u32) < target.h
                {
                    target.color[(s.y as u32 * target.w + s.x as u32) as usize] = pack(s.color);
                }
            }
        }
        Ok(())
    })();
    d.rt = rt;
    d.target = main;
    if r.is_ok() && use_gpu && !scratch.verts.is_empty() {
        let [t0, t1] = texs;
        let gtex = [
            t0.map(|t| (d.textures[0], t)),
            t1.map(|t| (d.textures[1], t)),
        ];
        // XP_D3D_PROBE=x,y: describe each draw covering that pixel (one frame in 120).
        if let Some((px, py)) = probe_point()
            && d.frames % 120 == 0
            && scratch.verts.chunks_exact(3).any(|t| covers(t, px, py))
        {
            let v = &scratch.verts[0];
            eprintln!(
                "[probe] frame {} fvf {:#x} verts {} viewport {:?} z/zw/zf {} {} {} blend {} {}->{} atest {} {} {:#x} cull {} tfactor {:#x} | stage0 {:?} tex {:?} | stage1 {:?} tex {:?} | v0 ({:.1},{:.1},{:.3},{:.3}) col {:?} uv {:?}",
                d.frames,
                d.shader,
                scratch.verts.len(),
                &d.viewport[..4],
                rs.zenable,
                rs.zwrite,
                rs.zfunc,
                rs.alpha_blend,
                rs.src_blend,
                rs.dst_blend,
                rs.alpha_test,
                rs.alpha_func,
                rs.alpha_ref,
                rs.cull,
                rs.texture_factor,
                (
                    stages[0].color_op,
                    stages[0].color_arg1,
                    stages[0].color_arg2,
                    stages[0].alpha_op,
                    stages[0].alpha_arg1,
                    stages[0].alpha_arg2,
                    stages[0].texcoord_index
                ),
                gtex[0].as_ref().map(|(id, t)| (
                    *id,
                    t.format,
                    t.levels.first().map(|l| (l.w, l.h))
                )),
                (
                    stages[1].color_op,
                    stages[1].color_arg1,
                    stages[1].color_arg2,
                    stages[1].alpha_op,
                    stages[1].alpha_arg1,
                    stages[1].alpha_arg2,
                    stages[1].texcoord_index
                ),
                gtex[1].as_ref().map(|(id, t)| (
                    *id,
                    t.format,
                    t.levels.first().map(|l| (l.w, l.h))
                )),
                v.x,
                v.y,
                v.z,
                v.rhw,
                v.color,
                v.uv
            );
            eprintln!(
                "[probe]   lighting {} ambient {:#x} material {:.2?} colorvertex {} srcs diff/amb/emi {} {} {} lights {:?}",
                d.rs[137],
                d.rs[139],
                d.material,
                d.rs[141],
                d.rs[145],
                d.rs[147],
                d.rs[148],
                setup
                    .lights
                    .iter()
                    .map(|l| (
                        l.directional,
                        l.diffuse,
                        l.ambient,
                        l.to_light,
                        l.range,
                        l.atten
                    ))
                    .collect::<Vec<_>>()
            );
            for (s, t) in gtex.iter().enumerate() {
                let Some((_, t)) = t else { continue };
                let Some(l) = t.levels.first() else { continue };
                let mut sum = [0.0f32; 4];
                let mut max = [0.0f32; 4];
                for j in 0..16 {
                    for i in 0..16 {
                        let c = texel(&e.mem, t.format, l, i * l.w / 16, j * l.h / 16);
                        for k in 0..4 {
                            sum[k] += c[k] / 256.0;
                            max[k] = max[k].max(c[k]);
                        }
                    }
                }
                eprintln!(
                    "[probe]   stage{s} texture {}x{} at {:#x}: mean {sum:.2?} max {max:.2?}",
                    l.w, l.h, l.ptr
                );
            }
        }
        if let Some(g) = d.gpu.as_mut() {
            g.draw(&e.mem, &rs, &stages, &gtex, &scratch.verts);
        }
    }
    d.scratch = scratch;
    r
}

/// A line or point as a one-pixel-wide quad (two triangles), for the GPU path.
pub(super) fn thin_quad(out: &mut Vec<Vertex>, a: &Vertex, b: &Vertex) {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    let (nx, ny) = if len > 1e-6 {
        (-dy / len * 0.5, dx / len * 0.5)
    } else {
        (0.0, 0.5)
    };
    let (ex, ey) = if len > 1e-6 {
        (dx / len * 0.5, dy / len * 0.5)
    } else {
        (0.5, 0.0)
    };
    let corner = |v: &Vertex, sx: f32, sy: f32| Vertex {
        x: v.x + sx,
        y: v.y + sy,
        ..*v
    };
    let (p0, p1) = (corner(a, nx - ex, ny - ey), corner(a, -nx - ex, -ny - ey));
    let (p2, p3) = (corner(b, nx + ex, ny + ey), corner(b, -nx + ex, -ny + ey));
    out.extend_from_slice(&[p0, p1, p2, p1, p3, p2]);
}

pub(super) fn draw_line(t: &mut Target, a: &Vertex, b: &Vertex, rs: &RenderState) {
    let steps = ((b.x - a.x).abs().max((b.y - a.y).abs()).ceil() as i32).max(1);
    for i in 0..=steps {
        let f = i as f32 / steps as f32;
        let (x, y) = (
            (a.x + (b.x - a.x) * f) as i32,
            (a.y + (b.y - a.y) * f) as i32,
        );
        if x < 0 || y < 0 || x >= t.w as i32 || y >= t.h as i32 {
            continue;
        }
        let idx = (y as u32 * t.w + x as u32) as usize;
        let z = a.z + (b.z - a.z) * f;
        if rs.zenable && !compare(rs.zfunc, z, t.depth[idx]) {
            continue;
        }
        let mut c = [0.0; 4];
        for k in 0..4 {
            c[k] = a.color[k] + (b.color[k] - a.color[k]) * f;
        }
        t.color[idx] = pack(c);
        if rs.zenable && rs.zwrite {
            t.depth[idx] = z;
        }
    }
}
