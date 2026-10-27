//! Metal backend for the Direct3D 8 pixel pipeline.
//!
//! `raster` is the reference: this module draws the same screen-space triangles with the
//! same fixed-function state (two texture stages, depth/alpha tests, blending), but on the
//! GPU. Triangles go into one command buffer per frame; the frame is only waited for when
//! the CPU needs the pixels back (Present, locking the back buffer).

use std::collections::HashMap;
use std::ffi::c_void;

use metal::*;

use crate::d3d::raster::{RenderState, StageState, TextureRef, Vertex, texel};
use crate::memory::Memory;

const SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VIn { float4 pos; float4 color; float4 spec; float2 uv0; float2 uv1; };
struct VOut {
    float4 position [[position]];
    float4 color;
    float4 spec;
    float2 uv0;
    float2 uv1;
};

// Vertices arrive in pixel coordinates with 1/w, like D3D's pre-transformed vertices.
vertex VOut vs_main(uint vid [[vertex_id]], const device VIn* verts [[buffer(0)]], constant float2& size [[buffer(1)]]) {
    VIn v = verts[vid];
    float w = (fabs(v.pos.w) > 1e-9) ? 1.0 / v.pos.w : 1.0;
    VOut o;
    o.position = float4((v.pos.x / size.x * 2.0 - 1.0) * w, (1.0 - v.pos.y / size.y * 2.0) * w, v.pos.z * w, w);
    o.color = v.color;
    o.spec = v.spec;
    o.uv0 = v.uv0;
    o.uv1 = v.uv1;
    return o;
}

struct Stage { uint cop, ca1, ca2, aop, aa1, aa2, tc, pad; };
struct FU {
    Stage stage[2];
    float4 tfactor;
    uint nstages;
    uint specular;
    uint atest;
    uint afunc;
    uint aref;
};

float4 pick(uint arg, float4 cur, float4 diffuse, float4 spec, float4 tex, float4 tf) {
    float4 v;
    switch (arg & 0xF) {
        case 0: v = diffuse; break;
        case 2: v = tex; break;
        case 3: v = tf; break;
        case 4: v = spec; break;
        default: v = cur; break;
    }
    if (arg & 0x10) v = 1.0 - v;
    if (arg & 0x20) v = float4(v.a, v.a, v.a, v.a);
    return v;
}

float4 combine(uint op, float4 x, float4 y, float4 cur, float4 diffuse, float4 tex, float4 tf) {
    switch (op) {
        case 2: return x;
        case 3: return y;
        case 4: return x * y;
        case 5: return x * y * 2.0;
        case 6: return x * y * 4.0;
        case 7: return x + y;
        case 8: return x + y - 0.5;
        case 9: return (x + y - 0.5) * 2.0;
        case 10: return x - y;
        case 11: return x + y - x * y;
        case 12: return x * diffuse.a + y * (1.0 - diffuse.a);
        case 13: return x * tex.a + y * (1.0 - tex.a);
        case 14: return x * tf.a + y * (1.0 - tf.a);
        case 15: return x + y * (1.0 - tex.a);
        case 16: return x * cur.a + y * (1.0 - cur.a);
        default: return x;
    }
}

bool compare_u(uint f, float a, float b) {
    switch (f) {
        case 1: return false;
        case 2: return a < b;
        case 3: return a == b;
        case 4: return a <= b;
        case 5: return a > b;
        case 6: return a != b;
        case 7: return a >= b;
        default: return true;
    }
}

fragment float4 fs_main(VOut in [[stage_in]], constant FU& u [[buffer(0)]],
                        texture2d<float> t0 [[texture(0)]], texture2d<float> t1 [[texture(1)]],
                        sampler s0 [[sampler(0)]], sampler s1 [[sampler(1)]]) {
    float4 cur = in.color;
    for (uint i = 0; i < u.nstages; i++) {
        Stage st = u.stage[i];
        float2 uv = st.tc == 0 ? in.uv0 : in.uv1;
        float4 tex = (i == 0) ? t0.sample(s0, uv) : t1.sample(s1, uv);
        float4 c1 = pick(st.ca1, cur, in.color, in.spec, tex, u.tfactor);
        float4 c2 = pick(st.ca2, cur, in.color, in.spec, tex, u.tfactor);
        float4 rgb = combine(st.cop, c1, c2, cur, in.color, tex, u.tfactor);
        float a = cur.a;
        if (st.aop != 1) {
            float4 a1 = pick(st.aa1, cur, in.color, in.spec, tex, u.tfactor);
            float4 a2 = pick(st.aa2, cur, in.color, in.spec, tex, u.tfactor);
            a = combine(st.aop, a1, a2, cur, in.color, tex, u.tfactor).a;
        }
        cur = float4(rgb.rgb, a);
    }
    if (u.specular != 0) cur.rgb += in.spec.rgb;
    if (u.atest != 0) {
        float a = floor(clamp(cur.a, 0.0, 1.0) * 255.0 + 0.5);
        if (!compare_u(u.afunc, a, float(u.aref & 0xFF))) discard_fragment();
    }
    return cur;
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuVertex {
    pos: [f32; 4],
    color: [f32; 4],
    spec: [f32; 4],
    uv0: [f32; 2],
    uv1: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StageU {
    cop: u32,
    ca1: u32,
    ca2: u32,
    aop: u32,
    aa1: u32,
    aa2: u32,
    tc: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FragU {
    stage: [StageU; 2],
    tfactor: [f32; 4],
    nstages: u32,
    specular: u32,
    atest: u32,
    afunc: u32,
    aref: u32,
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct PipeKey {
    blend: bool,
    src: u32,
    dst: u32,
    mask: u32,
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct SamplerKey {
    u: u32,
    v: u32,
    mag: u32,
    min: u32,
}

/// Size of one chunk of a frame's vertex ring (a frame takes more chunks as it needs them).
const RING_BYTES: usize = 32 << 20;

pub struct Gpu {
    device: Device,
    queue: CommandQueue,
    vs: Function,
    fs: Function,
    pipelines: HashMap<PipeKey, RenderPipelineState>,
    depth_states: HashMap<(bool, bool, u32), DepthStencilState>,
    samplers: HashMap<SamplerKey, SamplerState>,
    textures: HashMap<u32, Texture>,
    white: Texture,
    pub w: u32,
    pub h: u32,
    color: Texture,
    depth: Texture,
    /// CPU-visible copies of the color buffer, filled by a GPU blit (reading a GPU texture
    /// directly would make the driver decompress it on the CPU). There are two of these and
    /// of the vertex ring, so one frame can render while the CPU builds the next: index
    /// `cur` is the frame being built, and `pending` the submitted one being read back.
    /// A frame's ring is a list of chunks: one that fills up moves on to the next (made the
    /// first time a frame needs it) rather than waiting for the GPU.
    stage: [Buffer; 2],
    ring: [Vec<Buffer>; 2],
    cur: usize,
    pending: Option<CommandBuffer>,
    /// The chunk of `ring[cur]` being filled, and how far.
    chunk: usize,
    ring_off: usize,
    cmd: Option<CommandBuffer>,
    enc: Option<RenderCommandEncoder>,
    clear_color: Option<[f64; 4]>,
    clear_depth: Option<f32>,
    pub draws: u64,
}

fn new_texture(
    device: &Device,
    w: u32,
    h: u32,
    format: MTLPixelFormat,
    private: bool,
    target: bool,
) -> Texture {
    let d = TextureDescriptor::new();
    d.set_texture_type(MTLTextureType::D2);
    d.set_pixel_format(format);
    d.set_width(w.max(1) as u64);
    d.set_height(h.max(1) as u64);
    d.set_storage_mode(if private {
        MTLStorageMode::Private
    } else {
        MTLStorageMode::Shared
    });
    d.set_usage(if target {
        MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead
    } else {
        MTLTextureUsage::ShaderRead
    });
    device.new_texture(&d)
}

fn region(w: u32, h: u32) -> MTLRegion {
    MTLRegion {
        origin: MTLOrigin { x: 0, y: 0, z: 0 },
        size: MTLSize {
            width: w as u64,
            height: h as u64,
            depth: 1,
        },
    }
}

fn blend_factor(f: u32) -> MTLBlendFactor {
    match f {
        1 => MTLBlendFactor::Zero,
        3 => MTLBlendFactor::SourceColor,
        4 => MTLBlendFactor::OneMinusSourceColor,
        5 => MTLBlendFactor::SourceAlpha,
        6 => MTLBlendFactor::OneMinusSourceAlpha,
        7 => MTLBlendFactor::DestinationAlpha,
        8 => MTLBlendFactor::OneMinusDestinationAlpha,
        9 => MTLBlendFactor::DestinationColor,
        10 => MTLBlendFactor::OneMinusDestinationColor,
        11 => MTLBlendFactor::SourceAlphaSaturated,
        _ => MTLBlendFactor::One,
    }
}

fn compare_fn(f: u32) -> MTLCompareFunction {
    match f {
        1 => MTLCompareFunction::Never,
        2 => MTLCompareFunction::Less,
        3 => MTLCompareFunction::Equal,
        4 => MTLCompareFunction::LessEqual,
        5 => MTLCompareFunction::Greater,
        6 => MTLCompareFunction::NotEqual,
        7 => MTLCompareFunction::GreaterEqual,
        _ => MTLCompareFunction::Always,
    }
}

fn address_mode(m: u32) -> MTLSamplerAddressMode {
    match m {
        2 => MTLSamplerAddressMode::MirrorRepeat,
        3 | 4 => MTLSamplerAddressMode::ClampToEdge,
        _ => MTLSamplerAddressMode::Repeat,
    }
}

impl Gpu {
    /// None when there is no Metal device (or `XP_SOFTWARE` asks for the CPU renderer).
    pub fn new(w: u32, h: u32) -> Option<Gpu> {
        if std::env::var_os("XP_SOFTWARE").is_some() {
            return None;
        }
        let device = Device::system_default()?;
        let queue = device.new_command_queue();
        let lib = device
            .new_library_with_source(SHADERS, &CompileOptions::new())
            .map_err(|e| eprintln!("[metal] shader error: {e}"))
            .ok()?;
        let vs = lib.get_function("vs_main", None).ok()?;
        let fs = lib.get_function("fs_main", None).ok()?;
        let white = new_texture(&device, 1, 1, MTLPixelFormat::BGRA8Unorm, false, false);
        white.replace_region(region(1, 1), 0, [255u8; 4].as_ptr() as *const c_void, 4);
        let ring = [0; 2].map(|_| {
            vec![device.new_buffer(RING_BYTES as u64, MTLResourceOptions::StorageModeShared)]
        });
        let color = new_texture(&device, w, h, MTLPixelFormat::BGRA8Unorm, true, true);
        let stage = [0; 2].map(|_| {
            device.new_buffer(
                (w.max(1) * h.max(1) * 4) as u64,
                MTLResourceOptions::StorageModeShared,
            )
        });
        let depth = new_texture(&device, w, h, MTLPixelFormat::Depth32Float, true, true);
        Some(Gpu {
            device,
            queue,
            vs,
            fs,
            pipelines: HashMap::new(),
            depth_states: HashMap::new(),
            samplers: HashMap::new(),
            textures: HashMap::new(),
            white,
            w,
            h,
            color,
            depth,
            stage,
            ring,
            cur: 0,
            pending: None,
            chunk: 0,
            ring_off: 0,
            cmd: None,
            enc: None,
            clear_color: None,
            clear_depth: None,
            draws: 0,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.flush();
        self.pending = None;
        self.w = w;
        self.h = h;
        self.color = new_texture(&self.device, w, h, MTLPixelFormat::BGRA8Unorm, true, true);
        self.stage = [0; 2].map(|_| {
            self.device.new_buffer(
                (w.max(1) * h.max(1) * 4) as u64,
                MTLResourceOptions::StorageModeShared,
            )
        });
        self.depth = new_texture(&self.device, w, h, MTLPixelFormat::Depth32Float, true, true);
        self.clear_color = None;
        self.clear_depth = None;
    }

    /// Forget a texture so the next draw re-reads it from guest memory.
    pub fn invalidate_texture(&mut self, key: u32) {
        self.textures.remove(&key);
    }

    fn end_pass(&mut self) {
        if let Some(e) = self.enc.take() {
            e.end_encoding();
        }
    }

    fn ensure_encoder(&mut self) {
        if self.enc.is_some() {
            return;
        }
        let cmd = self
            .cmd
            .get_or_insert_with(|| self.queue.new_command_buffer().to_owned());
        let pass = RenderPassDescriptor::new();
        let c = pass.color_attachments().object_at(0).unwrap();
        c.set_texture(Some(&self.color));
        c.set_store_action(MTLStoreAction::Store);
        match self.clear_color.take() {
            Some(rgba) => {
                c.set_load_action(MTLLoadAction::Clear);
                // The back buffer is X8R8G8B8: it has no alpha, which reads back as 1.
                c.set_clear_color(MTLClearColor::new(rgba[0], rgba[1], rgba[2], 1.0));
            }
            None => c.set_load_action(MTLLoadAction::Load),
        }
        let d = pass.depth_attachment().unwrap();
        d.set_texture(Some(&self.depth));
        d.set_store_action(MTLStoreAction::Store);
        match self.clear_depth.take() {
            Some(z) => {
                d.set_load_action(MTLLoadAction::Clear);
                d.set_clear_depth(z as f64);
            }
            None => d.set_load_action(MTLLoadAction::Load),
        }
        self.enc = Some(cmd.new_render_command_encoder(pass).to_owned());
    }

    /// Finish everything submitted so far and wait for it.
    pub fn flush(&mut self) {
        if self.clear_color.is_some() || self.clear_depth.is_some() {
            self.ensure_encoder(); // applies the pending clears
        }
        self.end_pass();
        if let Some(c) = self.cmd.take() {
            c.commit();
            c.wait_until_completed();
        }
        (self.chunk, self.ring_off) = (0, 0);
    }

    /// Clear the whole color and/or depth buffer (ARGB color).
    pub fn clear(&mut self, color: Option<u32>, depth: Option<f32>) {
        self.end_pass(); // earlier draws must land before the clear
        if let Some(c) = color {
            let u = crate::d3d::raster::unpack(c);
            self.clear_color = Some([u[0] as f64, u[1] as f64, u[2] as f64, u[3] as f64]);
        }
        if depth.is_some() {
            self.clear_depth = depth;
        }
    }

    /// Copy the rendered frame into `out` as ARGB.
    pub fn read_color(&mut self, out: &mut [u32]) {
        let n = (self.w * self.h) as usize;
        if out.len() < n {
            return;
        }
        // Copy the frame into the staging buffer on the GPU, in the same command buffer as the
        // frame's drawing, then wait once.
        let cmd = self.submit_readback();
        cmd.wait_until_completed();
        (self.chunk, self.ring_off) = (0, 0);
        // SAFETY: the buffer holds w*h*4 bytes and the GPU is done with it.
        let src =
            unsafe { std::slice::from_raw_parts(self.stage[self.cur].contents() as *const u32, n) };
        out[..n].copy_from_slice(src);
    }

    /// Commit everything drawn so far plus a copy of the frame into `stage[cur]`.
    fn submit_readback(&mut self) -> CommandBuffer {
        if self.clear_color.is_some() || self.clear_depth.is_some() {
            self.ensure_encoder();
        }
        self.end_pass();
        let cmd = self
            .cmd
            .take()
            .unwrap_or_else(|| self.queue.new_command_buffer().to_owned());
        let blit = cmd.new_blit_command_encoder();
        let row = (self.w * 4) as u64;
        blit.copy_from_texture_to_buffer(
            &self.color,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            region(self.w, self.h).size,
            &self.stage[self.cur],
            0,
            row,
            row * self.h as u64,
            MTLBlitOption::empty(),
        );
        blit.end_encoding();
        cmd.commit();
        cmd
    }

    /// Submit the finished frame without waiting for it; `finish_frame` collects its pixels.
    /// A frame still being read back is collected (waiting if need be) first, into `out`.
    pub fn submit_frame(&mut self, out: &mut [u32]) -> bool {
        let collected = self.finish_frame(out, true);
        let cmd = self.submit_readback();
        self.pending = Some(cmd);
        // The next frame uses the other ring and staging buffer, whose last frame is done.
        self.cur ^= 1;
        (self.chunk, self.ring_off) = (0, 0);
        collected
    }

    /// Copy the submitted frame into `out` once the GPU has finished it; without `wait`, only
    /// if it already has. True when `out` was written.
    pub fn finish_frame(&mut self, out: &mut [u32], wait: bool) -> bool {
        let n = (self.w * self.h) as usize;
        let Some(cmd) = self.pending.as_ref() else {
            return false;
        };
        if !wait && cmd.status() != MTLCommandBufferStatus::Completed {
            return false;
        }
        cmd.wait_until_completed();
        self.pending = None;
        if out.len() < n {
            return false;
        }
        // SAFETY: the buffer holds w*h*4 bytes and the GPU is done with it.
        let src = unsafe {
            std::slice::from_raw_parts(self.stage[self.cur ^ 1].contents() as *const u32, n)
        };
        out[..n].copy_from_slice(src);
        true
    }

    /// Replace the color buffer with `px` (ARGB).
    pub fn write_color(&mut self, px: &[u32]) {
        let n = (self.w * self.h) as usize;
        if px.len() < n {
            return;
        }
        self.flush();
        // SAFETY: as above; nothing on the GPU is using the buffer after `flush`.
        unsafe { std::slice::from_raw_parts_mut(self.stage[self.cur].contents() as *mut u32, n) }
            .copy_from_slice(&px[..n]);
        let cmd = self.queue.new_command_buffer();
        let blit = cmd.new_blit_command_encoder();
        let row = (self.w * 4) as u64;
        blit.copy_from_buffer_to_texture(
            &self.stage[self.cur],
            0,
            row,
            row * self.h as u64,
            region(self.w, self.h).size,
            &self.color,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            MTLBlitOption::empty(),
        );
        blit.end_encoding();
        cmd.commit();
        cmd.wait_until_completed();
    }

    fn pipeline(&mut self, st: &RenderState) -> RenderPipelineState {
        let key = PipeKey {
            blend: st.alpha_blend,
            src: st.src_blend,
            dst: st.dst_blend,
            mask: st.color_write & 0xF,
        };
        if let Some(p) = self.pipelines.get(&key) {
            return p.clone();
        }
        let d = RenderPipelineDescriptor::new();
        d.set_vertex_function(Some(&self.vs));
        d.set_fragment_function(Some(&self.fs));
        d.set_depth_attachment_pixel_format(MTLPixelFormat::Depth32Float);
        let c = d.color_attachments().object_at(0).unwrap();
        c.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        let mut mask = MTLColorWriteMask::empty();
        // D3DCOLORWRITEENABLE: bit 0 red, 1 green, 2 blue, 3 alpha
        if key.mask & 1 != 0 {
            mask |= MTLColorWriteMask::Red;
        }
        if key.mask & 2 != 0 {
            mask |= MTLColorWriteMask::Green;
        }
        if key.mask & 4 != 0 {
            mask |= MTLColorWriteMask::Blue;
        }
        c.set_write_mask(mask);
        if key.blend {
            c.set_blending_enabled(true);
            c.set_rgb_blend_operation(MTLBlendOperation::Add);
            c.set_source_rgb_blend_factor(blend_factor(key.src));
            c.set_destination_rgb_blend_factor(blend_factor(key.dst));
            // The alpha channel keeps the shader's value, as in the reference rasterizer.
            c.set_alpha_blend_operation(MTLBlendOperation::Add);
            c.set_source_alpha_blend_factor(MTLBlendFactor::One);
            c.set_destination_alpha_blend_factor(MTLBlendFactor::Zero);
        }
        let p = self
            .device
            .new_render_pipeline_state(&d)
            .expect("metal pipeline");
        self.pipelines.insert(key, p.clone());
        p
    }

    fn depth_state(&mut self, st: &RenderState) -> DepthStencilState {
        let key = (st.zenable, st.zwrite, st.zfunc);
        if let Some(s) = self.depth_states.get(&key) {
            return s.clone();
        }
        let d = DepthStencilDescriptor::new();
        d.set_depth_compare_function(if st.zenable {
            compare_fn(st.zfunc)
        } else {
            MTLCompareFunction::Always
        });
        d.set_depth_write_enabled(st.zenable && st.zwrite);
        let s = self.device.new_depth_stencil_state(&d);
        self.depth_states.insert(key, s.clone());
        s
    }

    fn sampler(&mut self, stage: &StageState) -> SamplerState {
        let key = SamplerKey {
            u: stage.addr_u,
            v: stage.addr_v,
            mag: stage.mag,
            min: stage.min,
        };
        if let Some(s) = self.samplers.get(&key) {
            return s.clone();
        }
        let d = SamplerDescriptor::new();
        let filter = |f: u32| {
            if f == 1 {
                MTLSamplerMinMagFilter::Nearest
            } else {
                MTLSamplerMinMagFilter::Linear
            }
        };
        d.set_mag_filter(filter(stage.mag));
        d.set_min_filter(filter(stage.min));
        d.set_address_mode_s(address_mode(stage.addr_u));
        d.set_address_mode_t(address_mode(stage.addr_v));
        let s = self.device.new_sampler(&d);
        self.samplers.insert(key, s.clone());
        s
    }

    /// Upload (or reuse) the GPU copy of a guest texture's top level.
    fn texture(&mut self, mem: &Memory, key: u32, t: &TextureRef) -> Texture {
        if let Some(tex) = self.textures.get(&key) {
            return tex.clone();
        }
        let level = t.levels[0];
        let (w, h) = (level.w, level.h);
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let c = texel(mem, t.format, &level, x, y);
                let i = ((y * w + x) * 4) as usize;
                let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                bytes[i] = q(c[2]);
                bytes[i + 1] = q(c[1]);
                bytes[i + 2] = q(c[0]);
                bytes[i + 3] = q(c[3]);
            }
        }
        let tex = new_texture(&self.device, w, h, MTLPixelFormat::BGRA8Unorm, false, false);
        tex.replace_region(
            region(w, h),
            0,
            bytes.as_ptr() as *const c_void,
            (w * 4) as u64,
        );
        self.textures.insert(key, tex.clone());
        tex
    }

    /// Draw a triangle list (three vertices per triangle, pixel coordinates).
    /// `tex[i]` is the guest texture object id and its description for stage `i`.
    pub fn draw(
        &mut self,
        mem: &Memory,
        st: &RenderState,
        stages: &[StageState; 2],
        tex: &[Option<(u32, TextureRef)>; 2],
        verts: &[Vertex],
    ) {
        if verts.len() < 3 {
            return;
        }
        let bytes = verts.len() * std::mem::size_of::<GpuVertex>();
        if self.ring_off + bytes > RING_BYTES {
            self.chunk += 1;
            self.ring_off = 0;
            if self.ring[self.cur].len() <= self.chunk {
                let b = self
                    .device
                    .new_buffer(RING_BYTES as u64, MTLResourceOptions::StorageModeShared);
                self.ring[self.cur].push(b);
            }
        }
        if bytes > RING_BYTES {
            return;
        }
        // Stage setup mirrors the reference: a stage that reads a missing texture ends the cascade.
        let mut u = FragU {
            tfactor: crate::d3d::raster::unpack(st.texture_factor),
            specular: st.specular as u32,
            atest: st.alpha_test as u32,
            afunc: st.alpha_func,
            aref: st.alpha_ref,
            ..FragU::default()
        };
        for (s, stage) in stages.iter().enumerate() {
            if stage.color_op == 1 {
                break;
            }
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
            u.stage[s] = StageU {
                cop: stage.color_op,
                ca1: stage.color_arg1,
                ca2: stage.color_arg2,
                aop: stage.alpha_op,
                aa1: stage.alpha_arg1,
                aa2: stage.alpha_arg2,
                tc: (stage.texcoord_index & 0xFFFF).min(1),
                pad: 0,
            };
            u.nstages = s as u32 + 1;
        }
        let textures: Vec<Texture> = (0..2)
            .map(|s| match &tex[s] {
                Some((key, t)) if !t.levels.is_empty() => self.texture(mem, *key, t),
                _ => self.white.clone(),
            })
            .collect();
        let samplers = [self.sampler(&stages[0]), self.sampler(&stages[1])];
        let pipe = self.pipeline(st);
        let depth = self.depth_state(st);

        // Vertex data goes into the ring buffer.
        let base = self.ring_off;
        unsafe {
            let dst =
                (self.ring[self.cur][self.chunk].contents() as *mut u8).add(base) as *mut GpuVertex;
            for (i, v) in verts.iter().enumerate() {
                dst.add(i).write(GpuVertex {
                    pos: [v.x, v.y, v.z, v.rhw],
                    color: v.color,
                    spec: v.spec,
                    uv0: v.uv[0],
                    uv1: v.uv[1],
                });
            }
        }
        self.ring_off += (bytes + 255) & !255;

        self.ensure_encoder();
        let enc = self.enc.as_ref().unwrap();
        enc.set_render_pipeline_state(&pipe);
        enc.set_depth_stencil_state(&depth);
        enc.set_front_facing_winding(MTLWinding::Clockwise);
        enc.set_cull_mode(match st.cull {
            2 => MTLCullMode::Front,
            3 => MTLCullMode::Back,
            _ => MTLCullMode::None,
        });
        let [x0, y0, x1, y1] = st.scissor;
        let (x1, y1) = (x1.min(self.w), y1.min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        enc.set_scissor_rect(MTLScissorRect {
            x: x0 as u64,
            y: y0 as u64,
            width: (x1 - x0) as u64,
            height: (y1 - y0) as u64,
        });
        enc.set_vertex_buffer(0, Some(&self.ring[self.cur][self.chunk]), base as u64);
        let size = [self.w as f32, self.h as f32];
        enc.set_vertex_bytes(1, 8, size.as_ptr() as *const c_void);
        enc.set_fragment_bytes(
            0,
            std::mem::size_of::<FragU>() as u64,
            &u as *const FragU as *const c_void,
        );
        for s in 0..2 {
            enc.set_fragment_texture(s as u64, Some(&textures[s]));
            enc.set_fragment_sampler_state(s as u64, Some(&samplers[s]));
        }
        enc.draw_primitives(MTLPrimitiveType::Triangle, 0, verts.len() as u64);
        self.draws += 1;
    }
}

#[cfg(test)]
mod tests;
