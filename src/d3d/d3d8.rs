//! Direct3D 8, emulated.
//!
//! D3D8 objects are COM objects: a guest pointer to a block whose first word points at a
//! vtable of function pointers. Here each vtable slot points at a stub in the HLE region, so
//! `device->Clear(...)` in guest code lands in a Rust handler. Slot order must match the
//! DirectX 8 headers exactly, which is why each interface lists its methods in order with
//! their argument counts (including `this`).
//!
//! Rendering is done by the software rasterizer in `raster` (or on the GPU by `metal`); `Present` copies the frame
//! to the game's window.

use std::collections::HashMap;

use crate::cpu::{Emu, Res};
use crate::d3d::raster::*;
use crate::win32::{Args, Handler};

const S_OK: u32 = 0;
const E_NOINTERFACE: u32 = 0x8000_4002;
const D3DERR_INVALIDCALL: u32 = 0x8876_086C;
const D3DERR_NOTAVAILABLE: u32 = 0x8876_086A;

// ---- interfaces & vtables ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Iface {
    D3d,
    Device,
    Texture,
    Surface,
    VertexBuffer,
    IndexBuffer,
    SwapChain,
}

const ALL_IFACES: [Iface; 7] = [
    Iface::D3d,
    Iface::Device,
    Iface::Texture,
    Iface::Surface,
    Iface::VertexBuffer,
    Iface::IndexBuffer,
    Iface::SwapChain,
];

impl Iface {
    fn name(self) -> &'static str {
        match self {
            Iface::D3d => "IDirect3D8",
            Iface::Device => "IDirect3DDevice8",
            Iface::Texture => "IDirect3DTexture8",
            Iface::Surface => "IDirect3DSurface8",
            Iface::VertexBuffer => "IDirect3DVertexBuffer8",
            Iface::IndexBuffer => "IDirect3DIndexBuffer8",
            Iface::SwapChain => "IDirect3DSwapChain8",
        }
    }

    /// Methods in vtable order, with their argument counts including `this`.
    fn methods(self) -> &'static [(&'static str, usize)] {
        match self {
            Iface::D3d => D3D_METHODS,
            Iface::Device => DEVICE_METHODS,
            Iface::Texture => TEXTURE_METHODS,
            Iface::Surface => SURFACE_METHODS,
            Iface::VertexBuffer => VB_METHODS,
            Iface::IndexBuffer => IB_METHODS,
            Iface::SwapChain => SWAPCHAIN_METHODS,
        }
    }
}

const D3D_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("RegisterSoftwareDevice", 2),
    ("GetAdapterCount", 1),
    ("GetAdapterIdentifier", 4),
    ("GetAdapterModeCount", 2),
    ("EnumAdapterModes", 4),
    ("GetAdapterDisplayMode", 3),
    ("CheckDeviceType", 6),
    ("CheckDeviceFormat", 7),
    ("CheckDeviceMultiSampleType", 6),
    ("CheckDepthStencilMatch", 6),
    ("GetDeviceCaps", 4),
    ("GetAdapterMonitor", 2),
    ("CreateDevice", 7),
];

const DEVICE_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("TestCooperativeLevel", 1),
    ("GetAvailableTextureMem", 1),
    ("ResourceManagerDiscardBytes", 2),
    ("GetDirect3D", 2),
    ("GetDeviceCaps", 2),
    ("GetDisplayMode", 2),
    ("GetCreationParameters", 2),
    ("SetCursorProperties", 4),
    ("SetCursorPosition", 4),
    ("ShowCursor", 2),
    ("CreateAdditionalSwapChain", 3),
    ("Reset", 2),
    ("Present", 5),
    ("GetBackBuffer", 4),
    ("GetRasterStatus", 2),
    ("SetGammaRamp", 3),
    ("GetGammaRamp", 2),
    ("CreateTexture", 8),
    ("CreateVolumeTexture", 9),
    ("CreateCubeTexture", 7),
    ("CreateVertexBuffer", 6),
    ("CreateIndexBuffer", 6),
    ("CreateRenderTarget", 7),
    ("CreateDepthStencilSurface", 6),
    ("CreateImageSurface", 5),
    ("CopyRects", 6),
    ("UpdateTexture", 3),
    ("GetFrontBuffer", 2),
    ("SetRenderTarget", 3),
    ("GetRenderTarget", 2),
    ("GetDepthStencilSurface", 2),
    ("BeginScene", 1),
    ("EndScene", 1),
    ("Clear", 7),
    ("SetTransform", 3),
    ("GetTransform", 3),
    ("MultiplyTransform", 3),
    ("SetViewport", 2),
    ("GetViewport", 2),
    ("SetMaterial", 2),
    ("GetMaterial", 2),
    ("SetLight", 3),
    ("GetLight", 3),
    ("LightEnable", 3),
    ("GetLightEnable", 3),
    ("SetClipPlane", 3),
    ("GetClipPlane", 3),
    ("SetRenderState", 3),
    ("GetRenderState", 3),
    ("BeginStateBlock", 1),
    ("EndStateBlock", 2),
    ("ApplyStateBlock", 2),
    ("CaptureStateBlock", 2),
    ("DeleteStateBlock", 2),
    ("CreateStateBlock", 3),
    ("SetClipStatus", 2),
    ("GetClipStatus", 2),
    ("GetTexture", 3),
    ("SetTexture", 3),
    ("GetTextureStageState", 4),
    ("SetTextureStageState", 4),
    ("ValidateDevice", 2),
    ("GetInfo", 4),
    ("SetPaletteEntries", 3),
    ("GetPaletteEntries", 3),
    ("SetCurrentTexturePalette", 2),
    ("GetCurrentTexturePalette", 2),
    ("DrawPrimitive", 4),
    ("DrawIndexedPrimitive", 6),
    ("DrawPrimitiveUP", 5),
    ("DrawIndexedPrimitiveUP", 9),
    ("ProcessVertices", 6),
    ("CreateVertexShader", 5),
    ("SetVertexShader", 2),
    ("GetVertexShader", 2),
    ("DeleteVertexShader", 2),
    ("SetVertexShaderConstant", 4),
    ("GetVertexShaderConstant", 4),
    ("GetVertexShaderDeclaration", 4),
    ("GetVertexShaderFunction", 4),
    ("SetStreamSource", 4),
    ("GetStreamSource", 4),
    ("SetIndices", 3),
    ("GetIndices", 3),
    ("CreatePixelShader", 3),
    ("SetPixelShader", 2),
    ("GetPixelShader", 2),
    ("DeletePixelShader", 2),
    ("SetPixelShaderConstant", 4),
    ("GetPixelShaderConstant", 4),
    ("GetPixelShaderFunction", 4),
    ("DrawRectPatch", 4),
    ("DrawTriPatch", 4),
    ("DeletePatch", 2),
];

const RESOURCE_HEAD: [(&str, usize); 11] = [
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("GetDevice", 2),
    ("SetPrivateData", 5),
    ("GetPrivateData", 4),
    ("FreePrivateData", 2),
    ("SetPriority", 2),
    ("GetPriority", 1),
    ("PreLoad", 1),
    ("GetType", 1),
];

const TEXTURE_METHODS: &[(&str, usize)] = &[
    RESOURCE_HEAD[0],
    RESOURCE_HEAD[1],
    RESOURCE_HEAD[2],
    RESOURCE_HEAD[3],
    RESOURCE_HEAD[4],
    RESOURCE_HEAD[5],
    RESOURCE_HEAD[6],
    RESOURCE_HEAD[7],
    RESOURCE_HEAD[8],
    RESOURCE_HEAD[9],
    RESOURCE_HEAD[10],
    ("SetLOD", 2),
    ("GetLOD", 1),
    ("GetLevelCount", 1),
    ("GetLevelDesc", 3),
    ("GetSurfaceLevel", 3),
    ("LockRect", 5),
    ("UnlockRect", 2),
    ("AddDirtyRect", 2),
];

const SURFACE_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("GetDevice", 2),
    ("SetPrivateData", 5),
    ("GetPrivateData", 4),
    ("FreePrivateData", 2),
    ("GetContainer", 3),
    ("GetDesc", 2),
    ("LockRect", 4),
    ("UnlockRect", 1),
];

const VB_METHODS: &[(&str, usize)] = &[
    RESOURCE_HEAD[0],
    RESOURCE_HEAD[1],
    RESOURCE_HEAD[2],
    RESOURCE_HEAD[3],
    RESOURCE_HEAD[4],
    RESOURCE_HEAD[5],
    RESOURCE_HEAD[6],
    RESOURCE_HEAD[7],
    RESOURCE_HEAD[8],
    RESOURCE_HEAD[9],
    RESOURCE_HEAD[10],
    ("Lock", 5),
    ("Unlock", 1),
    ("GetDesc", 2),
];

const IB_METHODS: &[(&str, usize)] = VB_METHODS;

const SWAPCHAIN_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("Present", 5),
    ("GetBackBuffer", 4),
];

fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&d1.to_le_bytes());
    g[4..6].copy_from_slice(&d2.to_le_bytes());
    g[6..8].copy_from_slice(&d3.to_le_bytes());
    g[8..16].copy_from_slice(&d4);
    g
}

fn iid_of(i: Iface) -> [u8; 16] {
    match i {
        Iface::D3d => guid(
            0x1DD9E8DA,
            0x1C77,
            0x4D40,
            [0xB0, 0xCF, 0x98, 0xFE, 0xFD, 0xFF, 0x95, 0x12],
        ),
        Iface::Device => guid(
            0x7385E5DF,
            0x8FE8,
            0x41D5,
            [0x86, 0xB6, 0xD7, 0xB4, 0x85, 0x47, 0xB6, 0xCF],
        ),
        Iface::Texture => guid(
            0xE4CDD575,
            0x2866,
            0x4F01,
            [0xB1, 0x2E, 0x7E, 0xEC, 0xE1, 0xEC, 0x93, 0x58],
        ),
        Iface::Surface => guid(
            0xB96EEBCA,
            0xB326,
            0x4EA5,
            [0x88, 0x2F, 0x2F, 0xF5, 0xBA, 0xE0, 0x21, 0xDD],
        ),
        Iface::VertexBuffer => guid(
            0x8AEEEAC7,
            0x05F9,
            0x44D4,
            [0xB5, 0x91, 0x00, 0x0B, 0x0D, 0xF1, 0xCB, 0x95],
        ),
        Iface::IndexBuffer => guid(
            0x0E689C9A,
            0x053D,
            0x44A0,
            [0x9D, 0x92, 0xDB, 0x0E, 0x3D, 0x75, 0x0F, 0x86],
        ),
        Iface::SwapChain => guid(
            0x928C088B,
            0x76B9,
            0x4C6B,
            [0xA5, 0x36, 0xA5, 0x90, 0x85, 0x38, 0x76, 0xCD],
        ),
    }
}

const IID_IUNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
const IID_BASE_TEXTURE: [u8; 16] = [
    0xFA, 0x1C, 0x21, 0xB4, 0xB9, 0x51, 0x9F, 0x4A, 0xAB, 0x78, 0xDB, 0x99, 0xB2, 0xBB, 0x67, 0x8E,
];
const IID_RESOURCE: [u8; 16] = [
    0x7B, 0xBB, 0x36, 0x1B, 0xB7, 0x09, 0x0A, 0x41, 0xB4, 0x45, 0x7D, 0x14, 0x30, 0xD7, 0xB3, 0x3E,
];

// ---- objects ---------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SurfaceKind {
    BackBuffer,
    DepthStencil,
    /// Memory owned by the surface (render targets, image surfaces).
    Plain,
    /// A level of a texture: the memory belongs to the texture.
    TextureLevel,
}

#[derive(Clone)]
struct Surface {
    refs: u32,
    kind: SurfaceKind,
    format: u32,
    w: u32,
    h: u32,
    pitch: u32,
    ptr: u32,
    parent: u32,
}

struct Texture {
    refs: u32,
    format: u32,
    usage: u32,
    levels: Vec<Level>,
    surfaces: Vec<u32>,
}

struct Buffer {
    refs: u32,
    ptr: u32,
    len: u32,
    /// FVF for vertex buffers, D3DFMT_INDEX16/32 for index buffers.
    kind: u32,
    usage: u32,
}

enum Obj {
    D3d { refs: u32 },
    Device { refs: u32 },
    Texture(Texture),
    Surface(Surface),
    VertexBuffer(Buffer),
    IndexBuffer(Buffer),
}

impl Obj {
    fn refs(&mut self) -> &mut u32 {
        match self {
            Obj::D3d { refs } | Obj::Device { refs } => refs,
            Obj::Texture(t) => &mut t.refs,
            Obj::Surface(s) => &mut s.refs,
            Obj::VertexBuffer(b) | Obj::IndexBuffer(b) => &mut b.refs,
        }
    }
}

#[derive(Clone, Default)]
struct Params {
    width: u32,
    height: u32,
    format: u32,
    count: u32,
    hwnd: u32,
    /// Windowed (TRUE) or full screen at the back buffer's size.
    windowed: bool,
    auto_depth: bool,
    depth_format: u32,
}

struct Light {
    raw: [u8; 104],
    enabled: bool,
}

struct StateBlock {
    rs: Vec<(u32, u32)>,
    ts: Vec<(u32, u32, u32)>,
    textures: Vec<(u32, u32)>,
}

pub struct Device {
    hwnd: u32,
    params: Params,
    back_surface: u32,
    depth_surface: u32,
    target: Target,
    /// A texture or other surface being rendered to instead of the back buffer.
    rt: Option<(u32, Target)>,
    rs: [u32; 256],
    ts: [[u32; 32]; 8],
    world: [f32; 16],
    view: [f32; 16],
    proj: [f32; 16],
    tex_transform: [[f32; 16]; 8],
    viewport: [u32; 6],
    material: [f32; 17],
    lights: HashMap<u32, Light>,
    textures: [u32; 8],
    stream: (u32, u32),
    indices: (u32, u32),
    shader: u32,
    pub frames: u64,
    /// Wall-clock time of the last Present, and the gaps between Presents (ms) since someone
    /// last took them (`XP_FPS` reporting).
    last_present: Option<std::time::Instant>,
    pub frame_times: Vec<f32>,
    /// Metal renderer for the back buffer; None means the software rasterizer does it all.
    gpu: Option<crate::d3d::metal::Gpu>,
    scene_open: bool,
    recording: Option<StateBlock>,
    blocks: HashMap<u32, StateBlock>,
    next_token: u32,
    scratch: DrawScratch,
    /// The window of a frame Present submitted whose pixels have not been collected yet.
    pending_present: Option<u32>,
}

/// Buffers reused from draw to draw.
#[derive(Default)]
struct DrawScratch {
    /// Transformed vertices by vertex index, valid when the stamp is the current draw's, with
    /// where they land on screen (see `Projected`).
    cache: Vec<(u32, Projected, PipeVertex, Vertex)>,
    stamp: u32,
    verts: Vec<Vertex>,
}

/// A transformed vertex as triangles see it: already in screen space (XYZRHW), in front of
/// the near plane (its screen position is cached), or behind it (triangles using it are
/// clipped first).
#[derive(Clone, Copy, Default, PartialEq)]
enum Projected {
    #[default]
    Screen,
    Front,
    Behind,
}

fn identity() -> [f32; 16] {
    let mut m = [0.0; 16];
    for i in 0..4 {
        m[i * 5] = 1.0;
    }
    m
}

fn default_render_states() -> [u32; 256] {
    let mut r = [0u32; 256];
    r[7] = 1; // ZENABLE
    r[8] = 3; // FILLMODE solid
    r[9] = 2; // SHADEMODE gouraud
    r[14] = 1; // ZWRITEENABLE
    r[19] = 2; // SRCBLEND one
    r[20] = 1; // DESTBLEND zero
    r[22] = 3; // CULLMODE ccw
    r[23] = 4; // ZFUNC lessequal
    r[25] = 8; // ALPHAFUNC always
    r[26] = 1; // DITHERENABLE
    r[34] = 0; // FOGCOLOR
    r[35] = 0;
    r[36] = 0;
    r[37] = 1.0f32.to_bits();
    r[38] = 1.0f32.to_bits();
    r[60] = 0xFFFF_FFFF; // TEXTUREFACTOR
    r[136] = 1; // CLIPPING
    r[137] = 1; // LIGHTING
    r[141] = 1; // COLORVERTEX
    r[143] = 0; // NORMALIZENORMALS
    r[145] = 1; // DIFFUSEMATERIALSOURCE = COLOR1
    r[146] = 0; // SPECULAR: MATERIAL
    r[147] = 0; // AMBIENT: MATERIAL
    r[148] = 0; // EMISSIVE: MATERIAL
    r[168] = 0xF; // COLORWRITEENABLE
    r
}

fn default_stage_states() -> [[u32; 32]; 8] {
    let mut t = [[0u32; 32]; 8];
    for (s, stage) in t.iter_mut().enumerate() {
        let d = StageState::default_for(s);
        stage[1] = d.color_op;
        stage[2] = d.color_arg1;
        stage[3] = d.color_arg2;
        stage[4] = d.alpha_op;
        stage[5] = d.alpha_arg1;
        stage[6] = d.alpha_arg2;
        stage[11] = s as u32;
        stage[13] = 1; // ADDRESSU wrap
        stage[14] = 1;
        stage[16] = 1; // MAGFILTER point
        stage[17] = 1;
        stage[18] = 0; // MIPFILTER none
        stage[24] = 0;
    }
    t
}

#[derive(Default)]
pub struct D3d8 {
    objects: HashMap<u32, Obj>,
    vtables: HashMap<&'static str, u32>,
    d3d: u32,
    pub dev: Option<Device>,
    device_obj: u32,
    next_shader: u32,
    /// Textures whose guest memory changed (or that were destroyed); the GPU copy is stale.
    dirty: std::collections::HashSet<u32>,
}

impl D3d8 {
    pub fn frames(&self) -> u64 {
        self.dev.as_ref().map_or(0, |d| d.frames)
    }

    /// The most recently rendered frame: (w, h, ARGB pixels).
    #[cfg(test)]
    pub fn frame(&self) -> Option<(u32, u32, &[u32])> {
        self.dev
            .as_ref()
            .map(|d| (d.target.w, d.target.h, d.target.color.as_slice()))
    }
}

// ---- guest helpers --------------------------------------------------------------------------

fn read_f32(e: &Emu, p: u32) -> Res<f32> {
    Ok(f32::from_bits(e.mem.read(p, 4)?))
}

fn read_floats<const N: usize>(e: &Emu, p: u32) -> Res<[f32; N]> {
    let mut out = [0.0; N];
    for (i, v) in out.iter_mut().enumerate() {
        *v = read_f32(e, p + 4 * i as u32)?;
    }
    Ok(out)
}

fn write_floats(e: &mut Emu, p: u32, v: &[f32]) -> Res<()> {
    for (i, f) in v.iter().enumerate() {
        e.mem.write(p + 4 * i as u32, 4, f.to_bits())?;
    }
    Ok(())
}

fn vtable(e: &mut Emu, iface: Iface) -> Res<u32> {
    if let Some(&v) = e.win.d3d.vtables.get(iface.name()) {
        return Ok(v);
    }
    let methods = iface.methods();
    let base = e.heap_alloc(4 * methods.len() as u32)?;
    for (i, (m, _)) in methods.iter().enumerate() {
        let stub = e.win.resolve("d3d8", &format!("{}::{}", iface.name(), m));
        e.mem.write(base + 4 * i as u32, 4, stub)?;
    }
    e.win.d3d.vtables.insert(iface.name(), base);
    Ok(base)
}

/// Make a COM object: a guest block whose first word is the interface's vtable.
fn new_object(e: &mut Emu, iface: Iface, obj: Obj) -> Res<u32> {
    let vt = vtable(e, iface)?;
    let ptr = e.heap_alloc(16)?;
    e.mem.write(ptr, 4, vt)?;
    e.win.d3d.objects.insert(ptr, obj);
    Ok(ptr)
}

fn add_ref(e: &mut Emu, obj: u32) {
    if let Some(o) = e.win.d3d.objects.get_mut(&obj) {
        *o.refs() += 1;
    }
}

fn with_device<R>(e: &mut Emu, f: impl FnOnce(&mut Emu, &mut Device) -> Res<R>) -> Res<Option<R>> {
    let Some(mut dev) = e.win.d3d.dev.take() else {
        return Ok(None);
    };
    let r = f(e, &mut dev);
    e.win.d3d.dev = Some(dev);
    r.map(Some)
}

fn mip_chain(w: u32, h: u32, requested: u32) -> Vec<(u32, u32)> {
    let mut out = vec![(w, h)];
    let (mut cw, mut ch) = (w, h);
    while (requested == 0 || out.len() < requested as usize) && (cw > 1 || ch > 1) {
        cw = (cw / 2).max(1);
        ch = (ch / 2).max(1);
        out.push((cw, ch));
    }
    out
}

// ---- entry points ----------------------------------------------------------------------------

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "d3d8" {
        return None;
    }
    if name == "Direct3DCreate8" {
        return Some((1, direct3d_create8));
    }
    let (iface_name, method) = name.split_once("::")?;
    let iface = ALL_IFACES.into_iter().find(|i| i.name() == iface_name)?;
    let (_, argc) = *iface.methods().iter().find(|(m, _)| *m == method)?;
    Some((argc, handler_for(iface, method)))
}

fn direct3d_create8(e: &mut Emu, _: &Args) -> Res<u32> {
    if e.win.d3d.d3d != 0 {
        add_ref(e, e.win.d3d.d3d);
        return Ok(e.win.d3d.d3d);
    }
    let p = new_object(e, Iface::D3d, Obj::D3d { refs: 1 })?;
    e.win.d3d.d3d = p;
    Ok(p)
}

/// Methods nobody has needed yet succeed quietly; the trace shows the call.
fn ok(_: &mut Emu, _: &Args) -> Res<u32> {
    Ok(S_OK)
}

fn handler_for(iface: Iface, method: &str) -> Handler {
    match method {
        "QueryInterface" => query_interface,
        "AddRef" => |e, a| {
            add_ref(e, a[0]);
            Ok(e.win.d3d.objects.get_mut(&a[0]).map_or(1, |o| *o.refs()))
        },
        "Release" => release,
        _ => match iface {
            Iface::D3d => d3d_method(method),
            Iface::Device => device_method(method),
            Iface::Texture => texture_method(method),
            Iface::Surface => surface_method(method),
            Iface::VertexBuffer | Iface::IndexBuffer => buffer_method(method),
            Iface::SwapChain => match method {
                "Present" => |e, _| present(e, 0).map(|_| S_OK),
                "GetBackBuffer" => device_get_back_buffer_swap,
                _ => ok,
            },
        },
    }
}

fn query_interface(e: &mut Emu, a: &Args) -> Res<u32> {
    let want = e.mem.read_bytes(a[1], 16)?;
    let this_iface = e.win.d3d.objects.get(&a[0]).map(|o| match o {
        Obj::D3d { .. } => Iface::D3d,
        Obj::Device { .. } => Iface::Device,
        Obj::Texture(_) => Iface::Texture,
        Obj::Surface(_) => Iface::Surface,
        Obj::VertexBuffer(_) => Iface::VertexBuffer,
        Obj::IndexBuffer(_) => Iface::IndexBuffer,
    });
    let Some(iface) = this_iface else {
        return Ok(E_NOINTERFACE);
    };
    let matches = want == IID_IUNKNOWN
        || want == iid_of(iface)
        || (iface == Iface::Texture && (want == IID_BASE_TEXTURE || want == IID_RESOURCE))
        || (matches!(iface, Iface::VertexBuffer | Iface::IndexBuffer) && want == IID_RESOURCE);
    if matches {
        e.mem.write(a[2], 4, a[0])?;
        add_ref(e, a[0]);
        Ok(S_OK)
    } else {
        e.mem.write(a[2], 4, 0)?;
        Ok(E_NOINTERFACE)
    }
}

fn release(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(o) = e.win.d3d.objects.get_mut(&a[0]) else {
        return Ok(0);
    };
    let refs = o.refs();
    *refs = refs.saturating_sub(1);
    let left = *refs;
    if left == 0 {
        // Objects the device is holding keep their own references, so this is the last user.
        e.win.d3d.dirty.insert(a[0]);
        // Give the memory back, unless something still holds a piece of it.
        match e.win.d3d.objects.remove(&a[0]) {
            Some(Obj::Texture(t)) => {
                let levels_in_use = t.surfaces.iter().any(
                    |s| matches!(e.win.d3d.objects.get(s), Some(Obj::Surface(x)) if x.refs > 1),
                );
                for s in &t.surfaces {
                    e.win.d3d.objects.remove(s);
                }
                if !levels_in_use && let Some(l) = t.levels.first() {
                    e.free_virtual(l.ptr);
                }
            }
            Some(Obj::Surface(sf)) if sf.kind != SurfaceKind::TextureLevel && sf.ptr != 0 => {
                e.free_virtual(sf.ptr);
            }
            Some(Obj::VertexBuffer(b)) | Some(Obj::IndexBuffer(b)) => {
                e.free_virtual(b.ptr);
            }
            _ => {} // devices and the like: handled below
        }
        if a[0] == e.win.d3d.device_obj {
            e.win.d3d.dev = None;
            e.win.d3d.device_obj = 0;
        }
        if a[0] == e.win.d3d.d3d {
            e.win.d3d.d3d = 0;
        }
    }
    Ok(left)
}

// ---- IDirect3D8 ------------------------------------------------------------------------------

const MODES: [(u32, u32); 4] = [(640, 480), (800, 600), (1024, 768), (1280, 1024)];

fn color_format_ok(f: u32) -> bool {
    matches!(
        f,
        FMT_A8R8G8B8 | FMT_X8R8G8B8 | FMT_R5G6B5 | FMT_X1R5G5B5 | FMT_A1R5G5B5 | FMT_A4R4G4B4
    )
}

fn depth_format_ok(f: u32) -> bool {
    matches!(f, 70 | 71 | 73 | 75 | 77 | 79 | 80)
}

fn d3d_method(method: &str) -> Handler {
    match method {
        "GetAdapterCount" => |_, _| Ok(1),
        "GetAdapterIdentifier" => |e, a| {
            if a[1] != 0 {
                return Ok(D3DERR_INVALIDCALL);
            }
            let p = a[3];
            e.mem.write_bytes(p, &vec![0u8; 1068])?;
            e.mem.write_bytes(p, b"nv4_disp.dll\0")?;
            e.mem
                .write_bytes(p + 512, b"NVIDIA GeForce2 MX (emulated)\0")?;
            e.mem.write(p + 1024, 4, 0x0006_000E)?; // driver version, low
            e.mem.write(p + 1028, 4, 0x0006_000E)?;
            e.mem.write(p + 1032, 4, 0x10DE)?; // VendorId: NVIDIA
            e.mem.write(p + 1036, 4, 0x0110)?; // DeviceId: GeForce2 MX
            e.mem.write(p + 1048, 4, 0x1)?; // DeviceIdentifier (any non-zero)
            Ok(S_OK)
        },
        "GetAdapterModeCount" => |_, a| {
            Ok(if a[1] == 0 {
                (MODES.len() * 2) as u32
            } else {
                0
            })
        },
        "EnumAdapterModes" => |e, a| {
            let i = a[2] as usize;
            if a[1] != 0 || i >= MODES.len() * 2 {
                return Ok(D3DERR_INVALIDCALL);
            }
            let (w, h) = MODES[i / 2];
            let fmt = if i.is_multiple_of(2) {
                FMT_X8R8G8B8
            } else {
                FMT_R5G6B5
            };
            for (k, v) in [w, h, 60, fmt].iter().enumerate() {
                e.mem.write(a[3] + 4 * k as u32, 4, *v)?;
            }
            Ok(S_OK)
        },
        "GetAdapterDisplayMode" => |e, a| {
            for (k, v) in [1024u32, 768, 60, FMT_X8R8G8B8].iter().enumerate() {
                e.mem.write(a[2] + 4 * k as u32, 4, *v)?;
            }
            Ok(S_OK)
        },
        "CheckDeviceType" => |_, a| {
            Ok(
                if color_format_ok(a[3]) && (color_format_ok(a[4]) || a[4] == 0) {
                    S_OK
                } else {
                    D3DERR_NOTAVAILABLE
                },
            )
        },
        "CheckDeviceFormat" => |_, a| {
            // (this, adapter, devtype, adapter format, usage, resource type, check format)
            let (usage, rtype, fmt) = (a[4], a[5], a[6]);
            const USAGE_RENDERTARGET: u32 = 1;
            const USAGE_DEPTHSTENCIL: u32 = 2;
            let supported = if usage & USAGE_DEPTHSTENCIL != 0 {
                depth_format_ok(fmt)
            } else if usage & USAGE_RENDERTARGET != 0 {
                color_format_ok(fmt)
            } else {
                match rtype {
                    1 | 3 => {
                        color_format_ok(fmt)
                            || is_dxt(fmt)
                            || matches!(fmt, FMT_A8 | FMT_L8 | FMT_A8L8 | FMT_X4R4G4B4)
                    }
                    6 | 7 => true,
                    _ => false, // volume and cube textures are not supported
                }
            };
            Ok(if supported { S_OK } else { D3DERR_NOTAVAILABLE })
        },
        "CheckDeviceMultiSampleType" => {
            |_, a| Ok(if a[5] == 0 { S_OK } else { D3DERR_NOTAVAILABLE })
        }
        "CheckDepthStencilMatch" => |_, a| {
            Ok(if depth_format_ok(a[5]) {
                S_OK
            } else {
                D3DERR_NOTAVAILABLE
            })
        },
        "GetDeviceCaps" => |e, a| {
            fill_caps(e, a[3], a[2])?;
            Ok(S_OK)
        },
        "GetAdapterMonitor" => |_, _| Ok(0x0001_0001),
        "CreateDevice" => create_device,
        _ => ok,
    }
}

/// D3DCAPS8: a late-2002 DX7-class card with hardware T&L, two texture stages and no
/// programmable shaders, which is what the renderer here implements.
fn fill_caps(e: &mut Emu, p: u32, adapter_or_type: u32) -> Res<()> {
    let mut c = [0u32; 53];
    let f = |v: f32| v.to_bits();
    c[0] = 1; // HAL
    c[1] = 0;
    c[2] = 0x0002_0000; // READ_SCANLINE
    c[3] = 0x3002_0000; // CANMANAGERESOURCE | DYNAMICTEXTURES | FULLSCREENGAMMA
    c[4] = 0x20;
    c[5] = 0x8000_0001; // intervals: one | immediate
    c[6] = 1;
    c[7] = 0x0008_BFF0 | 0x0001_0000; // device caps incl. hardware transform and light
    c[8] = 0x6F2; // primitive misc
    c[9] = 0x0003_6190; // raster
    c[10] = 0xFF;
    c[11] = 0x1FFF;
    c[12] = 0x1FFF;
    c[13] = 0xFF;
    c[14] = 0x0008_4208;
    c[15] = 0x4405; // perspective, alpha, projected, mipmap
    c[16] = 0x0303_0300;
    c[19] = 0x1F;
    c[21] = 0x1F;
    c[22] = 2048;
    c[23] = 2048;
    c[24] = 256;
    c[25] = 2048;
    c[26] = 2048;
    c[27] = 1;
    c[28] = f(1.0e10);
    c[29] = f(-2048.0);
    c[30] = f(-2048.0);
    c[31] = f(2048.0);
    c[32] = f(2048.0);
    c[35] = 0x0008_0008; // FVF: 8 texture coordinate sets
    c[36] = 0x0000_FFFF; // texture ops
    c[37] = 2; // blend stages
    c[38] = 2; // simultaneous textures
    c[39] = 0x3A; // vertex processing: directional + positional lights, local viewer
    c[40] = 8; // active lights
    c[44] = f(64.0);
    c[45] = 0x000F_FFFF;
    c[46] = 0x0000_FFFF;
    c[47] = 1; // streams
    c[48] = 255;
    c[49] = 0xFFFE_0000; // vertex shader version 0.0: none
    c[51] = 0xFFFF_0000; // pixel shader version 0.0: none
    let _ = adapter_or_type;
    for (i, v) in c.iter().enumerate() {
        e.mem.write(p + 4 * i as u32, 4, *v)?;
    }
    Ok(())
}

fn read_params(e: &Emu, p: u32) -> Res<Params> {
    let r = |i: u32| e.mem.read(p + 4 * i, 4);
    Ok(Params {
        width: r(0)?,
        height: r(1)?,
        format: r(2)?,
        count: r(3)?,
        hwnd: r(6)?,
        windowed: r(7)? != 0,
        auto_depth: r(8)? != 0,
        depth_format: r(9)?,
    })
}

/// A full-screen device sets the display mode to its back buffer's size: the screen takes
/// that size, and the device window covers it, as on Windows. A windowed one gives the
/// desktop back.
fn full_screen_mode(e: &mut Emu, p: &Params, hwnd: u32) -> Res<()> {
    if p.windowed {
        e.wm.set_screen(crate::gui::wm::DESKTOP_W, crate::gui::wm::DESKTOP_H);
        return Ok(());
    }
    e.wm.set_screen(p.width, p.height);
    if hwnd != 0 {
        crate::gui::user32::move_window(e, hwnd, 0, 0, p.width as i32, p.height as i32)?;
    }
    Ok(())
}

fn create_device(e: &mut Emu, a: &Args) -> Res<u32> {
    // (this, adapter, device type, focus window, behaviour flags, params*, IDirect3DDevice8**)
    if e.win.d3d.dev.is_some() {
        return Ok(D3DERR_INVALIDCALL); // one device at a time
    }
    // Unless asked to preserve it (D3DCREATE_FPU_PRESERVE), Direct3D puts the calling thread's
    // x87 into single precision, rounding to nearest: from then on the program's floating
    // point arithmetic is all single precision, which its results depend on.
    if a[4] & 2 == 0 {
        e.fpu.cw &= !0x0F00;
    }
    let mut p = read_params(e, a[5])?;
    let hwnd = if p.hwnd != 0 { p.hwnd } else { a[3] };
    if p.width == 0 || p.height == 0 {
        let (w, h) = e.wm.client_size(hwnd).unwrap_or((640, 480));
        p.width = w.max(1) as u32;
        p.height = h.max(1) as u32;
    }
    if p.format == 0 {
        p.format = FMT_X8R8G8B8;
    }
    p.count = p.count.max(1);
    full_screen_mode(e, &p, hwnd)?;
    // Report the values actually used, as Direct3D does.
    for (i, v) in [p.width, p.height, p.format, p.count].iter().enumerate() {
        e.mem.write(a[5] + 4 * i as u32, 4, *v)?;
    }
    let back = new_object(
        e,
        Iface::Surface,
        Obj::Surface(Surface {
            refs: 1,
            kind: SurfaceKind::BackBuffer,
            format: p.format,
            w: p.width,
            h: p.height,
            pitch: p.width * 4,
            ptr: 0,
            parent: 0,
        }),
    )?;
    let depth = if p.auto_depth {
        new_object(
            e,
            Iface::Surface,
            Obj::Surface(Surface {
                refs: 1,
                kind: SurfaceKind::DepthStencil,
                format: p.depth_format,
                w: p.width,
                h: p.height,
                pitch: p.width * 4,
                ptr: 0,
                parent: 0,
            }),
        )?
    } else {
        0
    };
    let mut rs = default_render_states();
    rs[7] = p.auto_depth as u32; // ZENABLE follows EnableAutoDepthStencil
    let dev = Device {
        hwnd,
        target: Target::new(p.width, p.height),
        params: p.clone(),
        back_surface: back,
        depth_surface: depth,
        rt: None,
        rs,
        ts: default_stage_states(),
        world: identity(),
        view: identity(),
        proj: identity(),
        tex_transform: [identity(); 8],
        viewport: [0, 0, p.width, p.height, 0.0f32.to_bits(), 1.0f32.to_bits()],
        material: [0.0; 17],
        lights: HashMap::new(),
        textures: [0; 8],
        stream: (0, 0),
        indices: (0, 0),
        shader: 0x142, // D3DFVF_XYZ | D3DFVF_DIFFUSE | D3DFVF_TEX1... a harmless default
        frames: 0,
        last_present: None,
        frame_times: Vec::new(),
        gpu: crate::d3d::metal::Gpu::new(p.width, p.height),
        scene_open: false,
        recording: None,
        blocks: HashMap::new(),
        next_token: 1,
        scratch: DrawScratch::default(),
        pending_present: None,
    };
    let obj = new_object(e, Iface::Device, Obj::Device { refs: 1 })?;
    e.win.d3d.dev = Some(dev);
    e.win.d3d.device_obj = obj;
    e.mem.write(a[6], 4, obj)?;
    Ok(S_OK)
}

// ---- resources ---------------------------------------------------------------------------------

fn create_texture(e: &mut Emu, a: &Args) -> Res<u32> {
    // (this, width, height, levels, usage, format, pool, IDirect3DTexture8**)
    let (w, h, levels, usage, format) = (a[1], a[2], a[3], a[4], a[5]);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return Ok(D3DERR_INVALIDCALL);
    }
    if !(color_format_ok(format)
        || is_dxt(format)
        || matches!(format, FMT_A8 | FMT_L8 | FMT_A8L8 | FMT_X4R4G4B4))
    {
        return Ok(D3DERR_NOTAVAILABLE);
    }
    let chain = mip_chain(w, h, levels);
    let total: u32 = chain
        .iter()
        .map(|&(cw, ch)| surface_size(format, cw, ch).1)
        .sum();
    let base = e.alloc_virtual(total.max(4));
    let mut off = 0;
    let mut lv = Vec::new();
    for (cw, ch) in chain {
        let (pitch, size) = surface_size(format, cw, ch);
        lv.push(Level {
            ptr: base + off,
            w: cw,
            h: ch,
            pitch,
        });
        off += size;
    }
    let n = lv.len();
    let p = new_object(
        e,
        Iface::Texture,
        Obj::Texture(Texture {
            refs: 1,
            format,
            usage,
            levels: lv,
            surfaces: vec![0; n],
        }),
    )?;
    e.mem.write(a[7], 4, p)?;
    Ok(S_OK)
}

fn texture_surface(e: &mut Emu, tex: u32, level: u32) -> Res<Option<u32>> {
    let (existing, fmt, lv) = match e.win.d3d.objects.get(&tex) {
        Some(Obj::Texture(t)) => match t.levels.get(level as usize) {
            Some(l) => (t.surfaces[level as usize], t.format, *l),
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    if existing != 0 {
        return Ok(Some(existing));
    }
    let s = new_object(
        e,
        Iface::Surface,
        Obj::Surface(Surface {
            refs: 1,
            kind: SurfaceKind::TextureLevel,
            format: fmt,
            w: lv.w,
            h: lv.h,
            pitch: lv.pitch,
            ptr: lv.ptr,
            parent: tex,
        }),
    )?;
    if let Some(Obj::Texture(t)) = e.win.d3d.objects.get_mut(&tex) {
        t.surfaces[level as usize] = s;
    }
    Ok(Some(s))
}

/// Pointer to a locked rectangle's first byte and the pitch.
fn lock_level(lv: &Level, format: u32, rect: Option<(i32, i32)>) -> (u32, u32) {
    let off = match rect {
        Some((x, y)) if !is_dxt(format) => {
            y.max(0) as u32 * lv.pitch + x.max(0) as u32 * bytes_per_pixel(format)
        }
        _ => 0,
    };
    (lv.ptr + off, lv.pitch)
}

fn texture_method(method: &str) -> Handler {
    match method {
        "GetLevelCount" => |e, a| {
            Ok(match e.win.d3d.objects.get(&a[0]) {
                Some(Obj::Texture(t)) => t.levels.len() as u32,
                _ => 0,
            })
        },
        "GetLevelDesc" => |e, a| {
            let Some(Obj::Texture(t)) = e.win.d3d.objects.get(&a[0]) else {
                return Ok(D3DERR_INVALIDCALL);
            };
            let Some(l) = t.levels.get(a[1] as usize).copied() else {
                return Ok(D3DERR_INVALIDCALL);
            };
            let (fmt, usage) = (t.format, t.usage);
            // D3DSURFACE_DESC: Format, Type, Usage, Pool, Size, MultiSampleType, Width, Height
            let (_, size) = surface_size(fmt, l.w, l.h);
            for (i, v) in [fmt, 1, usage, 1, size, 0, l.w, l.h].iter().enumerate() {
                e.mem.write(a[2] + 4 * i as u32, 4, *v)?;
            }
            Ok(S_OK)
        },
        "GetSurfaceLevel" => |e, a| match texture_surface(e, a[0], a[1])? {
            Some(s) => {
                add_ref(e, s);
                e.mem.write(a[2], 4, s)?;
                Ok(S_OK)
            }
            None => Ok(D3DERR_INVALIDCALL),
        },
        "LockRect" => |e, a| {
            // (this, level, D3DLOCKED_RECT*, RECT*, flags)
            let Some(Obj::Texture(t)) = e.win.d3d.objects.get(&a[0]) else {
                return Ok(D3DERR_INVALIDCALL);
            };
            let Some(lv) = t.levels.get(a[1] as usize).copied() else {
                return Ok(D3DERR_INVALIDCALL);
            };
            let fmt = t.format;
            let rect = if a[3] != 0 {
                Some((e.mem.read(a[3], 4)? as i32, e.mem.read(a[3] + 4, 4)? as i32))
            } else {
                None
            };
            let (ptr, pitch) = lock_level(&lv, fmt, rect);
            e.mem.write(a[2], 4, pitch)?;
            e.mem.write(a[2] + 4, 4, ptr)?;
            mark_dirty(e, a[0]);
            Ok(S_OK)
        },
        "UnlockRect" => |e, a| {
            mark_dirty(e, a[0]);
            Ok(S_OK)
        },
        "SetLOD" | "GetLOD" | "GetPriority" | "SetPriority" | "PreLoad" => |_, _| Ok(0),
        "GetType" => |_, _| Ok(3), // D3DRTYPE_TEXTURE
        "GetDevice" => |e, a| {
            e.mem.write(a[1], 4, e.win.d3d.device_obj)?;
            Ok(S_OK)
        },
        _ => ok,
    }
}

fn surface_method(method: &str) -> Handler {
    match method {
        "GetDesc" => |e, a| {
            let Some(Obj::Surface(s)) = e.win.d3d.objects.get(&a[0]) else {
                return Ok(D3DERR_INVALIDCALL);
            };
            let (fmt, w, h) = (s.format, s.w, s.h);
            let (_, size) = surface_size(fmt, w, h);
            let usage = if s.kind == SurfaceKind::BackBuffer {
                1
            } else if s.kind == SurfaceKind::DepthStencil {
                2
            } else {
                0
            };
            for (i, v) in [
                fmt,
                1,
                usage,
                if s.kind == SurfaceKind::Plain { 3 } else { 0 },
                size,
                0,
                w,
                h,
            ]
            .iter()
            .enumerate()
            {
                e.mem.write(a[1] + 4 * i as u32, 4, *v)?;
            }
            Ok(S_OK)
        },
        "LockRect" => |e, a| {
            // (this, D3DLOCKED_RECT*, RECT*, flags)
            let Some(s) = surface_snapshot(e, a[0]) else {
                return Ok(D3DERR_INVALIDCALL);
            };
            let mut s = s;
            mark_dirty(e, a[0]);
            if s.kind == SurfaceKind::BackBuffer {
                sync_back_buffer(e);
            }
            if s.kind == SurfaceKind::BackBuffer || s.kind == SurfaceKind::DepthStencil {
                // Give the guest a copy of the pixels; UnlockRect puts them back.
                if s.ptr == 0 {
                    s.ptr = e.alloc_virtual(s.w * s.h * 4);
                    s.pitch = s.w * 4;
                    if let Some(Obj::Surface(o)) = e.win.d3d.objects.get_mut(&a[0]) {
                        o.ptr = s.ptr;
                        o.pitch = s.pitch;
                    }
                }
                if s.kind == SurfaceKind::BackBuffer
                    && let Some(Some(())) = Some(with_device(e, |e, d| {
                        for (i, px) in d.target.color.iter().enumerate() {
                            e.mem.write(s.ptr + 4 * i as u32, 4, *px)?;
                        }
                        Ok(())
                    })?)
                {}
            }
            let lv = Level {
                ptr: s.ptr,
                w: s.w,
                h: s.h,
                pitch: s.pitch,
            };
            let rect = if a[2] != 0 {
                Some((e.mem.read(a[2], 4)? as i32, e.mem.read(a[2] + 4, 4)? as i32))
            } else {
                None
            };
            let (ptr, pitch) = lock_level(&lv, s.format, rect);
            e.mem.write(a[1], 4, pitch)?;
            e.mem.write(a[1] + 4, 4, ptr)?;
            Ok(S_OK)
        },
        "UnlockRect" => |e, a| {
            let Some(s) = surface_snapshot(e, a[0]) else {
                return Ok(D3DERR_INVALIDCALL);
            };
            if s.kind == SurfaceKind::BackBuffer && s.ptr != 0 {
                with_device(e, |e, d| {
                    for i in 0..d.target.color.len() {
                        d.target.color[i] = e.mem.read(s.ptr + 4 * i as u32, 4)?;
                    }
                    if let Some(g) = d.gpu.as_mut() {
                        g.write_color(&d.target.color);
                    }
                    Ok(())
                })?;
            }
            mark_dirty(e, a[0]);
            Ok(S_OK)
        },
        "GetContainer" => |e, a| {
            let parent = match e.win.d3d.objects.get(&a[0]) {
                Some(Obj::Surface(s)) if s.parent != 0 => s.parent,
                _ => return Ok(D3DERR_INVALIDCALL),
            };
            add_ref(e, parent);
            e.mem.write(a[2], 4, parent)?;
            Ok(S_OK)
        },
        "GetDevice" => |e, a| {
            e.mem.write(a[1], 4, e.win.d3d.device_obj)?;
            Ok(S_OK)
        },
        _ => ok,
    }
}

fn buffer_method(method: &str) -> Handler {
    match method {
        "Lock" => |e, a| {
            // (this, offset, size, BYTE** data, flags)
            let ptr = match e.win.d3d.objects.get(&a[0]) {
                Some(Obj::VertexBuffer(b)) | Some(Obj::IndexBuffer(b)) => b.ptr,
                _ => return Ok(D3DERR_INVALIDCALL),
            };
            e.mem.write(a[3], 4, ptr + a[1])?;
            Ok(S_OK)
        },
        "Unlock" => |_, _| Ok(S_OK),
        "GetDesc" => |e, a| {
            let (b_ptr, len, kind, usage, is_vb) = match e.win.d3d.objects.get(&a[0]) {
                Some(Obj::VertexBuffer(b)) => (b.ptr, b.len, b.kind, b.usage, true),
                Some(Obj::IndexBuffer(b)) => (b.ptr, b.len, b.kind, b.usage, false),
                _ => return Ok(D3DERR_INVALIDCALL),
            };
            let _ = b_ptr;
            // VERTEXBUFFER_DESC: Format, Type, Usage, Pool, Size, FVF
            let fmt = if is_vb { 100 } else { kind };
            for (i, v) in [
                fmt,
                if is_vb { 6 } else { 7 },
                usage,
                1,
                len,
                if is_vb { kind } else { 0 },
            ]
            .iter()
            .enumerate()
            {
                e.mem.write(a[1] + 4 * i as u32, 4, *v)?;
            }
            Ok(S_OK)
        },
        "GetType" => |e, a| {
            Ok(
                if matches!(e.win.d3d.objects.get(&a[0]), Some(Obj::VertexBuffer(_))) {
                    6
                } else {
                    7
                },
            )
        },
        "GetDevice" => |e, a| {
            e.mem.write(a[1], 4, e.win.d3d.device_obj)?;
            Ok(S_OK)
        },
        "SetPriority" | "GetPriority" | "PreLoad" => |_, _| Ok(0),
        _ => ok,
    }
}

// ---- IDirect3DDevice8 -----------------------------------------------------------------------

fn device_get_back_buffer_swap(e: &mut Emu, a: &Args) -> Res<u32> {
    let back = e.win.d3d.dev.as_ref().map_or(0, |d| d.back_surface);
    if back == 0 {
        return Ok(D3DERR_INVALIDCALL);
    }
    add_ref(e, back);
    e.mem.write(a[3], 4, back)?;
    Ok(S_OK)
}

fn record_rs(d: &mut Device, state: u32, value: u32) {
    if let Some(rec) = d.recording.as_mut() {
        rec.rs.push((state, value));
    }
}

fn device_method(method: &str) -> Handler {
    match method {
        "TestCooperativeLevel" => |_, _| Ok(S_OK),
        "GetAvailableTextureMem" => |_, _| Ok(128 << 20),
        "GetDirect3D" => |e, a| {
            let d3d = e.win.d3d.d3d;
            add_ref(e, d3d);
            e.mem.write(a[1], 4, d3d)?;
            Ok(S_OK)
        },
        "GetDeviceCaps" => |e, a| {
            fill_caps(e, a[1], 0)?;
            Ok(S_OK)
        },
        "GetDisplayMode" => |e, a| {
            for (k, v) in [1024u32, 768, 60, FMT_X8R8G8B8].iter().enumerate() {
                e.mem.write(a[1] + 4 * k as u32, 4, *v)?;
            }
            Ok(S_OK)
        },
        "GetCreationParameters" => |e, a| {
            let hwnd = e.win.d3d.dev.as_ref().map_or(0, |d| d.hwnd);
            for (k, v) in [0u32, 1, hwnd, 0x40].iter().enumerate() {
                e.mem.write(a[1] + 4 * k as u32, 4, *v)?;
            }
            Ok(S_OK)
        },
        "ShowCursor" => |_, _| Ok(0),
        "Reset" => |e, a| {
            let p = read_params(e, a[1])?;
            let hwnd = e.win.d3d.dev.as_ref().map_or(0, |d| d.hwnd);
            if p.width != 0 && p.height != 0 {
                full_screen_mode(e, &p, if p.hwnd != 0 { p.hwnd } else { hwnd })?;
            }
            with_device(e, |_, d| {
                if p.width != 0 && p.height != 0 {
                    d.target = Target::new(p.width, p.height);
                    if let Some(g) = d.gpu.as_mut() {
                        g.resize(p.width, p.height);
                    }
                    d.params.width = p.width;
                    d.params.height = p.height;
                    d.viewport = [0, 0, p.width, p.height, 0.0f32.to_bits(), 1.0f32.to_bits()];
                }
                Ok(())
            })?;
            Ok(S_OK)
        },
        "Present" => |e, a| present(e, a[3]).map(|_| S_OK),
        "GetBackBuffer" => |e, a| {
            let back = e.win.d3d.dev.as_ref().map_or(0, |d| d.back_surface);
            if back == 0 || a[1] != 0 {
                return Ok(D3DERR_INVALIDCALL);
            }
            add_ref(e, back);
            e.mem.write(a[3], 4, back)?;
            Ok(S_OK)
        },
        "GetRasterStatus" => |e, a| {
            let frames = e.win.d3d.frames();
            e.mem.write(a[1], 4, 0)?; // InVBlank
            e.mem.write(a[1] + 4, 4, (frames % 768) as u32)?;
            Ok(S_OK)
        },
        "CreateTexture" => create_texture,
        "CreateVertexBuffer" => |e, a| {
            // (this, length, usage, FVF, pool, IDirect3DVertexBuffer8**)
            if a[1] == 0 || a[1] > 64 << 20 {
                return Ok(D3DERR_INVALIDCALL);
            }
            let ptr = e.alloc_virtual(a[1]);
            let p = new_object(
                e,
                Iface::VertexBuffer,
                Obj::VertexBuffer(Buffer {
                    refs: 1,
                    ptr,
                    len: a[1],
                    kind: a[3],
                    usage: a[2],
                }),
            )?;
            e.mem.write(a[5], 4, p)?;
            Ok(S_OK)
        },
        "CreateIndexBuffer" => |e, a| {
            // (this, length, usage, format, pool, IDirect3DIndexBuffer8**)
            if a[1] == 0 || a[1] > 64 << 20 {
                return Ok(D3DERR_INVALIDCALL);
            }
            let ptr = e.alloc_virtual(a[1]);
            let p = new_object(
                e,
                Iface::IndexBuffer,
                Obj::IndexBuffer(Buffer {
                    refs: 1,
                    ptr,
                    len: a[1],
                    kind: a[3],
                    usage: a[2],
                }),
            )?;
            e.mem.write(a[5], 4, p)?;
            Ok(S_OK)
        },
        "CreateRenderTarget" | "CreateImageSurface" => |e, a| {
            // CreateRenderTarget(this, w, h, format, multisample, lockable, out)
            // CreateImageSurface(this, w, h, format, out)
            let (w, h, fmt) = (a[1], a[2], a[3]);
            let out = if a[6] != 0 || a[5] > 1 { a[6] } else { a[4] };
            let out = if a[5] <= 1 && a[6] == 0 { a[4] } else { out };
            if w == 0 || h == 0 || w > 4096 || h > 4096 {
                return Ok(D3DERR_INVALIDCALL);
            }
            let (pitch, size) = surface_size(fmt, w, h);
            let ptr = e.alloc_virtual(size.max(4));
            let s = new_object(
                e,
                Iface::Surface,
                Obj::Surface(Surface {
                    refs: 1,
                    kind: SurfaceKind::Plain,
                    format: fmt,
                    w,
                    h,
                    pitch,
                    ptr,
                    parent: 0,
                }),
            )?;
            e.mem.write(out, 4, s)?;
            Ok(S_OK)
        },
        "CreateDepthStencilSurface" => |e, a| {
            let s = new_object(
                e,
                Iface::Surface,
                Obj::Surface(Surface {
                    refs: 1,
                    kind: SurfaceKind::DepthStencil,
                    format: a[3],
                    w: a[1],
                    h: a[2],
                    pitch: a[1] * 4,
                    ptr: 0,
                    parent: 0,
                }),
            )?;
            e.mem.write(a[5], 4, s)?;
            Ok(S_OK)
        },
        "CreateVolumeTexture" | "CreateCubeTexture" | "CreateAdditionalSwapChain" => {
            |_, _| Ok(D3DERR_NOTAVAILABLE)
        }
        "CopyRects" => copy_rects,
        "UpdateTexture" => update_texture,
        "GetFrontBuffer" => |e, a| {
            // Copy the displayed frame into an image surface.
            let Some(s) = surface_snapshot(e, a[1]) else {
                return Ok(D3DERR_INVALIDCALL);
            };
            sync_back_buffer(e);
            with_device(e, |e, d| {
                let bpp = bytes_per_pixel(s.format).max(1);
                for y in 0..s.h.min(d.target.h) {
                    for x in 0..s.w.min(d.target.w) {
                        let px = d.target.color[(y * d.target.w + x) as usize];
                        e.mem.write(
                            s.ptr + y * s.pitch + x * bpp,
                            bpp.min(4),
                            store_pixel(s.format, px),
                        )?;
                    }
                }
                Ok(())
            })?;
            Ok(S_OK)
        },
        "SetRenderTarget" => set_render_target,
        "GetRenderTarget" => |e, a| {
            let (rt, back) = e.win.d3d.dev.as_ref().map_or((0, 0), |d| {
                (d.rt.as_ref().map_or(0, |r| r.0), d.back_surface)
            });
            let s = if rt != 0 { rt } else { back };
            add_ref(e, s);
            e.mem.write(a[1], 4, s)?;
            Ok(S_OK)
        },
        "GetDepthStencilSurface" => |e, a| {
            let d = e.win.d3d.dev.as_ref().map_or(0, |d| d.depth_surface);
            if d == 0 {
                return Ok(D3DERR_NOTAVAILABLE);
            }
            add_ref(e, d);
            e.mem.write(a[1], 4, d)?;
            Ok(S_OK)
        },
        "BeginScene" => |e, _| {
            with_device(e, |_, d| {
                d.scene_open = true;
                Ok(())
            })?;
            Ok(S_OK)
        },
        "EndScene" => |e, _| {
            with_device(e, |e, d| {
                d.scene_open = false;
                flush_render_target(e, d)
            })?;
            Ok(S_OK)
        },
        "Clear" => clear,
        "SetTransform" | "MultiplyTransform" => |e, a| {
            let m = read_floats::<16>(e, a[2])?;
            with_device(e, |_, d| {
                let slot = match a[1] {
                    2 => Some(&mut d.view),
                    3 => Some(&mut d.proj),
                    256 => Some(&mut d.world),
                    16..=23 => Some(&mut d.tex_transform[a[1] as usize - 16]),
                    _ => None,
                };
                if let Some(slot) = slot {
                    *slot = m;
                }
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetTransform" => |e, a| {
            let m = e.win.d3d.dev.as_ref().map(|d| match a[1] {
                2 => d.view,
                3 => d.proj,
                16..=23 => d.tex_transform[a[1] as usize - 16],
                _ => d.world,
            });
            write_floats(e, a[2], &m.unwrap_or_else(identity))?;
            Ok(S_OK)
        },
        "SetViewport" => |e, a| {
            let mut v = [0u32; 6];
            for (i, slot) in v.iter_mut().enumerate() {
                *slot = e.mem.read(a[1] + 4 * i as u32, 4)?;
            }
            with_device(e, |_, d| {
                d.viewport = v;
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetViewport" => |e, a| {
            let v = e.win.d3d.dev.as_ref().map_or([0; 6], |d| d.viewport);
            for (i, x) in v.iter().enumerate() {
                e.mem.write(a[1] + 4 * i as u32, 4, *x)?;
            }
            Ok(S_OK)
        },
        "SetMaterial" => |e, a| {
            let m = read_floats::<17>(e, a[1])?;
            with_device(e, |_, d| {
                d.material = m;
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetMaterial" => |e, a| {
            let m = e.win.d3d.dev.as_ref().map_or([0.0; 17], |d| d.material);
            write_floats(e, a[1], &m)?;
            Ok(S_OK)
        },
        "SetLight" => |e, a| {
            let mut raw = [0u8; 104];
            raw.copy_from_slice(&e.mem.read_bytes(a[2], 104)?);
            with_device(e, |_, d| {
                let enabled = d.lights.get(&a[1]).is_some_and(|l| l.enabled);
                d.lights.insert(a[1], Light { raw, enabled });
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetLight" => |e, a| {
            let raw = e
                .win
                .d3d
                .dev
                .as_ref()
                .and_then(|d| d.lights.get(&a[1]).map(|l| l.raw));
            match raw {
                Some(r) => {
                    e.mem.write_bytes(a[2], &r)?;
                    Ok(S_OK)
                }
                None => Ok(D3DERR_INVALIDCALL),
            }
        },
        "LightEnable" => |e, a| {
            with_device(e, |_, d| {
                d.lights
                    .entry(a[1])
                    .or_insert(Light {
                        raw: [0; 104],
                        enabled: false,
                    })
                    .enabled = a[2] != 0;
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetLightEnable" => |e, a| {
            let on = e
                .win
                .d3d
                .dev
                .as_ref()
                .and_then(|d| d.lights.get(&a[1]).map(|l| l.enabled))
                .unwrap_or(false);
            e.mem.write(a[2], 4, on as u32)?;
            Ok(S_OK)
        },
        "SetRenderState" => |e, a| {
            with_device(e, |_, d| {
                let s = (a[1] & 0xFF) as usize;
                d.rs[s] = a[2];
                record_rs(d, a[1], a[2]);
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetRenderState" => |e, a| {
            let v = e
                .win
                .d3d
                .dev
                .as_ref()
                .map_or(0, |d| d.rs[(a[1] & 0xFF) as usize]);
            e.mem.write(a[2], 4, v)?;
            Ok(S_OK)
        },
        "SetTextureStageState" => |e, a| {
            with_device(e, |_, d| {
                if let Some(stage) = d.ts.get_mut(a[1] as usize)
                    && let Some(slot) = stage.get_mut(a[2] as usize)
                {
                    *slot = a[3];
                }
                if let Some(rec) = d.recording.as_mut() {
                    rec.ts.push((a[1], a[2], a[3]));
                }
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetTextureStageState" => |e, a| {
            let v = e
                .win
                .d3d
                .dev
                .as_ref()
                .and_then(|d| {
                    d.ts.get(a[1] as usize)
                        .and_then(|s| s.get(a[2] as usize))
                        .copied()
                })
                .unwrap_or(0);
            e.mem.write(a[3], 4, v)?;
            Ok(S_OK)
        },
        "SetTexture" => |e, a| {
            if a[1] >= 8 {
                return Ok(D3DERR_INVALIDCALL);
            }
            let old = e
                .win
                .d3d
                .dev
                .as_ref()
                .map_or(0, |d| d.textures[a[1] as usize]);
            if a[2] != 0 {
                add_ref(e, a[2]);
            }
            with_device(e, |_, d| {
                d.textures[a[1] as usize] = a[2];
                if let Some(rec) = d.recording.as_mut() {
                    rec.textures.push((a[1], a[2]));
                }
                Ok(())
            })?;
            if old != 0 {
                let mut args: Args = [0; crate::win32::MAX_ARGS];
                args[0] = old;
                release(e, &args)?;
            }
            Ok(S_OK)
        },
        "GetTexture" => |e, a| {
            let t = e
                .win
                .d3d
                .dev
                .as_ref()
                .map_or(0, |d| d.textures.get(a[1] as usize).copied().unwrap_or(0));
            if t != 0 {
                add_ref(e, t);
            }
            e.mem.write(a[2], 4, t)?;
            Ok(S_OK)
        },
        "ValidateDevice" => |e, a| {
            e.mem.write(a[1], 4, 1)?;
            Ok(S_OK)
        },
        "BeginStateBlock" => |e, _| {
            with_device(e, |_, d| {
                d.recording = Some(StateBlock {
                    rs: vec![],
                    ts: vec![],
                    textures: vec![],
                });
                Ok(())
            })?;
            Ok(S_OK)
        },
        "EndStateBlock" => |e, a| {
            let token = with_device(e, |_, d| {
                let t = d.next_token;
                d.next_token += 1;
                if let Some(rec) = d.recording.take() {
                    d.blocks.insert(t, rec);
                }
                Ok(t)
            })?
            .unwrap_or(0);
            e.mem.write(a[1], 4, token)?;
            Ok(S_OK)
        },
        "CreateStateBlock" => |e, a| {
            let token = with_device(e, |_, d| {
                let t = d.next_token;
                d.next_token += 1;
                let mut rec = StateBlock {
                    rs: vec![],
                    ts: vec![],
                    textures: vec![],
                };
                for (i, v) in d.rs.iter().enumerate() {
                    rec.rs.push((i as u32, *v));
                }
                for (s, stage) in d.ts.iter().enumerate() {
                    for (k, v) in stage.iter().enumerate() {
                        rec.ts.push((s as u32, k as u32, *v));
                    }
                }
                d.blocks.insert(t, rec);
                Ok(t)
            })?
            .unwrap_or(0);
            e.mem.write(a[2], 4, token)?;
            Ok(S_OK)
        },
        "ApplyStateBlock" => |e, a| {
            with_device(e, |_, d| {
                if let Some(b) = d.blocks.get(&a[1]) {
                    let (rs, ts, tex) = (b.rs.clone(), b.ts.clone(), b.textures.clone());
                    for (s, v) in rs {
                        d.rs[(s & 0xFF) as usize] = v;
                    }
                    for (s, k, v) in ts {
                        if let Some(slot) =
                            d.ts.get_mut(s as usize)
                                .and_then(|st| st.get_mut(k as usize))
                        {
                            *slot = v;
                        }
                    }
                    for (s, t) in tex {
                        d.textures[s as usize & 7] = t;
                    }
                }
                Ok(())
            })?;
            Ok(S_OK)
        },
        "DeleteStateBlock" => |e, a| {
            with_device(e, |_, d| {
                d.blocks.remove(&a[1]);
                Ok(())
            })?;
            Ok(S_OK)
        },
        "DrawPrimitive" => |e, a| {
            draw(
                e,
                &DrawCall::Stream {
                    ptype: a[1],
                    start: a[2],
                    count: a[3],
                    indexed: None,
                },
            )
        },
        "DrawIndexedPrimitive" => |e, a| {
            draw(
                e,
                &DrawCall::Stream {
                    ptype: a[1],
                    start: a[2],
                    count: a[5],
                    indexed: Some((a[4], a[3], a[2])),
                },
            )
        },
        "DrawPrimitiveUP" => |e, a| {
            draw(
                e,
                &DrawCall::UserData {
                    ptype: a[1],
                    count: a[2],
                    vertices: a[3],
                    stride: a[4],
                    indices: None,
                },
            )
        },
        "DrawIndexedPrimitiveUP" => |e, a| {
            // (this, type, minIndex, numVertices, primCount, indexData, indexFormat, vertexData, stride)
            draw(
                e,
                &DrawCall::UserData {
                    ptype: a[1],
                    count: a[4],
                    vertices: a[7],
                    stride: a[8],
                    indices: Some((a[5], a[6])),
                },
            )
        },
        "CreateVertexShader" | "CreatePixelShader" => |e, a| {
            // Programmable shaders are not supported (the caps say so); hand out a handle anyway.
            let h = 0x8000_0000 | {
                e.win.d3d.next_shader += 1;
                e.win.d3d.next_shader
            };
            let out = if a[4] != 0 || a[2] == 0 { a[3] } else { a[2] };
            e.mem.write(out, 4, h)?;
            Ok(S_OK)
        },
        "SetVertexShader" => |e, a| {
            with_device(e, |_, d| {
                d.shader = a[1];
                Ok(())
            })?;
            Ok(S_OK)
        },
        "GetVertexShader" => |e, a| {
            let s = e.win.d3d.dev.as_ref().map_or(0, |d| d.shader);
            e.mem.write(a[1], 4, s)?;
            Ok(S_OK)
        },
        "SetStreamSource" => |e, a| {
            if a[1] != 0 {
                return Ok(D3DERR_INVALIDCALL); // one stream is all the caps promise
            }
            let old = e.win.d3d.dev.as_ref().map_or(0, |d| d.stream.0);
            if a[2] != 0 {
                add_ref(e, a[2]);
            }
            with_device(e, |_, d| {
                d.stream = (a[2], a[3]);
                Ok(())
            })?;
            if old != 0 {
                let mut args: Args = [0; crate::win32::MAX_ARGS];
                args[0] = old;
                release(e, &args)?;
            }
            Ok(S_OK)
        },
        "GetStreamSource" => |e, a| {
            let (vb, stride) = e.win.d3d.dev.as_ref().map_or((0, 0), |d| d.stream);
            if vb != 0 {
                add_ref(e, vb);
            }
            e.mem.write(a[2], 4, vb)?;
            if a[3] != 0 {
                e.mem.write(a[3], 4, stride)?;
            }
            Ok(S_OK)
        },
        "SetIndices" => |e, a| {
            let old = e.win.d3d.dev.as_ref().map_or(0, |d| d.indices.0);
            if a[1] != 0 {
                add_ref(e, a[1]);
            }
            with_device(e, |_, d| {
                d.indices = (a[1], a[2]);
                Ok(())
            })?;
            if old != 0 {
                let mut args: Args = [0; crate::win32::MAX_ARGS];
                args[0] = old;
                release(e, &args)?;
            }
            Ok(S_OK)
        },
        "GetIndices" => |e, a| {
            let (ib, base) = e.win.d3d.dev.as_ref().map_or((0, 0), |d| d.indices);
            if ib != 0 {
                add_ref(e, ib);
            }
            e.mem.write(a[1], 4, ib)?;
            if a[2] != 0 {
                e.mem.write(a[2], 4, base)?;
            }
            Ok(S_OK)
        },
        _ => ok,
    }
}

// ---- surface copies & render targets ---------------------------------------------------------

fn surface_snapshot(e: &Emu, ptr: u32) -> Option<Surface> {
    match e.win.d3d.objects.get(&ptr) {
        Some(Obj::Surface(s)) => Some(s.clone()),
        _ => None,
    }
}

fn copy_surface_memory(
    e: &mut Emu,
    src: &Surface,
    dst: &Surface,
    rect: Option<(i32, i32, i32, i32)>,
    point: (u32, u32),
) -> Res<()> {
    if src.format != dst.format {
        return Ok(()); // format conversion on copy is not supported
    }
    let (x0, y0, x1, y1) = rect.map_or((0, 0, src.w as i32, src.h as i32), |r| r);
    if is_dxt(src.format) {
        // Whole-surface copy is the common case for compressed textures.
        let (_, size) = surface_size(src.format, src.w.min(dst.w), src.h.min(dst.h));
        let bytes = e.mem.read_bytes(src.ptr, size)?;
        e.mem.write_bytes(dst.ptr, &bytes)?;
        return Ok(());
    }
    let bpp = bytes_per_pixel(src.format).max(1);
    let (w, h) = ((x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32);
    for row in 0..h {
        if y0 as u32 + row >= src.h || point.1 + row >= dst.h {
            break;
        }
        let cols = w
            .min(src.w.saturating_sub(x0 as u32))
            .min(dst.w.saturating_sub(point.0));
        let from = src.ptr + (y0 as u32 + row) * src.pitch + x0 as u32 * bpp;
        let to = dst.ptr + (point.1 + row) * dst.pitch + point.0 * bpp;
        let bytes = e.mem.read_bytes(from, cols * bpp)?;
        e.mem.write_bytes(to, &bytes)?;
    }
    Ok(())
}

fn copy_rects(e: &mut Emu, a: &Args) -> Res<u32> {
    // (this, src surface, src rects*, count, dst surface, dst points*)
    mark_dirty(e, a[4]);
    let (Some(src), Some(dst)) = (surface_snapshot(e, a[1]), surface_snapshot(e, a[4])) else {
        return Ok(D3DERR_INVALIDCALL);
    };
    if src.ptr == 0 || dst.ptr == 0 {
        return Ok(S_OK);
    }
    if a[3] == 0 || a[2] == 0 {
        copy_surface_memory(e, &src, &dst, None, (0, 0))?;
        return Ok(S_OK);
    }
    for i in 0..a[3] {
        let r = (
            e.mem.read(a[2] + 16 * i, 4)? as i32,
            e.mem.read(a[2] + 16 * i + 4, 4)? as i32,
            e.mem.read(a[2] + 16 * i + 8, 4)? as i32,
            e.mem.read(a[2] + 16 * i + 12, 4)? as i32,
        );
        let pt = if a[5] != 0 {
            (
                e.mem.read(a[5] + 8 * i, 4)?,
                e.mem.read(a[5] + 8 * i + 4, 4)?,
            )
        } else {
            (r.0 as u32, r.1 as u32)
        };
        copy_surface_memory(e, &src, &dst, Some(r), pt)?;
    }
    Ok(S_OK)
}

fn update_texture(e: &mut Emu, a: &Args) -> Res<u32> {
    let (src, dst) = match (e.win.d3d.objects.get(&a[1]), e.win.d3d.objects.get(&a[2])) {
        (Some(Obj::Texture(s)), Some(Obj::Texture(d))) => {
            (s.levels.clone(), (d.levels.clone(), d.format))
        }
        _ => return Ok(D3DERR_INVALIDCALL),
    };
    // Match levels by size: the destination may have fewer levels than the source.
    for dl in &dst.0 {
        if let Some(sl) = src.iter().find(|l| l.w == dl.w && l.h == dl.h) {
            let (_, size) = surface_size(dst.1, dl.w, dl.h);
            let bytes = e.mem.read_bytes(sl.ptr, size)?;
            e.mem.write_bytes(dl.ptr, &bytes)?;
        }
    }
    mark_dirty(e, a[2]);
    Ok(S_OK)
}

/// Encode an ARGB pixel into `format`'s storage.
fn store_pixel(format: u32, argb: u32) -> u32 {
    let (a, r, g, b) = (
        (argb >> 24) & 0xFF,
        (argb >> 16) & 0xFF,
        (argb >> 8) & 0xFF,
        argb & 0xFF,
    );
    match format {
        FMT_R5G6B5 => ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3),
        FMT_X1R5G5B5 => ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3),
        FMT_A1R5G5B5 => ((a >> 7) << 15) | ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3),
        FMT_A4R4G4B4 => ((a >> 4) << 12) | ((r >> 4) << 8) | ((g >> 4) << 4) | (b >> 4),
        _ => argb,
    }
}

/// Write a texture render target's pixels back into its guest memory.
fn flush_render_target(e: &mut Emu, d: &mut Device) -> Res<()> {
    let Some((surf, target)) = d.rt.as_ref() else {
        return Ok(());
    };
    let Some(s) = surface_snapshot(e, *surf) else {
        return Ok(());
    };
    mark_dirty(e, *surf);
    let bpp = bytes_per_pixel(s.format).max(1);
    for y in 0..s.h.min(target.h) {
        for x in 0..s.w.min(target.w) {
            let px = target.color[(y * target.w + x) as usize];
            e.mem.write(
                s.ptr + y * s.pitch + x * bpp,
                bpp.min(4),
                store_pixel(s.format, px),
            )?;
        }
    }
    Ok(())
}

fn set_render_target(e: &mut Emu, a: &Args) -> Res<u32> {
    // (this, render target surface, depth surface)
    let new = a[1];
    with_device(e, |e, d| {
        flush_render_target(e, d)?;
        if new == 0 || new == d.back_surface {
            d.rt = None;
        } else if let Some(Obj::Surface(s)) = e.win.d3d.objects.get(&new) {
            d.rt = Some((new, Target::new(s.w, s.h)));
        }
        Ok(())
    })?;
    Ok(S_OK)
}

// ---- Clear & Present -------------------------------------------------------------------------

fn clear(e: &mut Emu, a: &Args) -> Res<u32> {
    // (this, count, rects*, flags, color, z, stencil)
    let (count, rects, flags, color, z) = (a[1], a[2], a[3], a[4], f32::from_bits(a[5]));
    let mut regions = Vec::new();
    if count > 0 && rects != 0 {
        for i in 0..count {
            regions.push((
                e.mem.read(rects + 16 * i, 4)? as i32,
                e.mem.read(rects + 16 * i + 4, 4)? as i32,
                e.mem.read(rects + 16 * i + 8, 4)? as i32,
                e.mem.read(rects + 16 * i + 12, 4)? as i32,
            ));
        }
    }
    with_device(e, |e, d| {
        if d.rt.is_none() && d.gpu.is_some() {
            let g = d.gpu.as_mut().unwrap();
            if regions.is_empty() {
                g.clear(
                    (flags & 1 != 0).then_some(color),
                    (flags & 2 != 0).then_some(z),
                );
            } else {
                // A rectangle clear is a quad that ignores the depth test and writes the clear values.
                let rs = RenderState {
                    zenable: flags & 2 != 0,
                    zwrite: flags & 2 != 0,
                    zfunc: 8,
                    cull: 1,
                    color_write: if flags & 1 != 0 { 0xF } else { 0 },
                    ..RenderState::default()
                };
                let stg = [
                    StageState {
                        color_op: 1,
                        ..StageState::default_for(0)
                    },
                    StageState::default_for(1),
                ];
                for (x0, y0, x1, y1) in &regions {
                    let v = |x: i32, y: i32| Vertex {
                        x: x as f32,
                        y: y as f32,
                        z,
                        rhw: 1.0,
                        color: unpack(color),
                        ..Vertex::default()
                    };
                    let quad = [
                        v(*x0, *y0),
                        v(*x1, *y0),
                        v(*x0, *y1),
                        v(*x1, *y0),
                        v(*x1, *y1),
                        v(*x0, *y1),
                    ];
                    g.draw(&e.mem, &rs, &stg, &[None, None], &quad);
                }
            }
            return Ok(());
        }
        let t = match d.rt.as_mut() {
            Some((_, t)) => t,
            None => &mut d.target,
        };
        if regions.is_empty() {
            t.clear(
                (flags & 1 != 0).then_some(color),
                (flags & 2 != 0).then_some(z),
            );
        } else {
            for (x0, y0, x1, y1) in &regions {
                for y in (*y0).max(0)..(*y1).min(t.h as i32) {
                    for x in (*x0).max(0)..(*x1).min(t.w as i32) {
                        let i = (y as u32 * t.w + x as u32) as usize;
                        if flags & 1 != 0 {
                            t.color[i] = color;
                        }
                        if flags & 2 != 0 {
                            t.depth[i] = z;
                        }
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(S_OK)
}

/// Bring the CPU copy of the back buffer up to date with what the GPU has drawn.
fn sync_back_buffer(e: &mut Emu) {
    finish_present(e, true);
    if let Some(d) = e.win.d3d.dev.as_mut()
        && let Some(g) = d.gpu.as_mut()
        && d.target.color.len() == (g.w * g.h) as usize
    {
        g.read_color(&mut d.target.color);
    }
}

/// The texture a texture or one of its level surfaces belongs to has new contents.
fn mark_dirty(e: &mut Emu, obj: u32) {
    let key = match e.win.d3d.objects.get(&obj) {
        Some(Obj::Surface(s)) if s.parent != 0 => s.parent,
        _ => obj,
    };
    e.win.d3d.dirty.insert(key);
}

/// Show the back buffer: copy it into the surface of the game's window.
///
/// With the GPU, the frame is only submitted here: its pixels reach the window when it is
/// done (`finish_present`), so the CPU goes on with the next frame meanwhile.
fn present(e: &mut Emu, override_hwnd: u32) -> Res<()> {
    let Some(hwnd) = e.win.d3d.dev.as_ref().map(|d| {
        if override_hwnd != 0 {
            override_hwnd
        } else {
            d.hwnd
        }
    }) else {
        return Ok(());
    };
    // Some(the previous frame's window, if that frame was collected just now)
    let submitted = e.win.d3d.dev.as_mut().and_then(|d| {
        let g = d
            .gpu
            .as_mut()
            .filter(|g| d.target.color.len() == (g.w * g.h) as usize)?;
        let collected = g.submit_frame(&mut d.target.color);
        Some(std::mem::replace(&mut d.pending_present, Some(hwnd)).filter(|_| collected))
    });
    match submitted {
        Some(Some(prev)) => show_frame(e, prev),
        Some(None) => {}
        None => show_frame(e, hwnd),
    }
    if let Some(d) = e.win.d3d.dev.as_mut() {
        d.frames += 1;
        let now = std::time::Instant::now();
        if let Some(prev) = d.last_present.replace(now) {
            d.frame_times.push((now - prev).as_secs_f32() * 1000.0);
        }
    }
    Ok(())
}

/// Put a frame submitted by Present into its window once the GPU has finished it (without
/// `wait`, only if it already has).
pub fn finish_present(e: &mut Emu, wait: bool) {
    let Some(d) = e.win.d3d.dev.as_mut() else {
        return;
    };
    let Some(hwnd) = d.pending_present else {
        return;
    };
    let Some(g) = d.gpu.as_mut() else { return };
    if g.finish_frame(&mut d.target.color, wait) {
        d.pending_present = None;
        show_frame(e, hwnd);
    } else if wait {
        d.pending_present = None;
    }
}

/// Copy (scaling if the sizes differ) the device's frame into the window.
fn show_frame(e: &mut Emu, hwnd: u32) {
    let Some((w, h)) = e.win.d3d.dev.as_ref().map(|d| (d.target.w, d.target.h)) else {
        return;
    };
    if let (Some(d), Some(win)) = (e.win.d3d.dev.as_ref(), e.wm.window_mut(hwnd)) {
        let pixels = &d.target.color;
        let (sw, sh) = (win.surface.width as usize, win.surface.height as usize);
        let (w, h) = (w as usize, h as usize);
        if sw > 0 && sh > 0 && pixels.len() >= w * h {
            let xmap: Vec<usize> = (0..sw).map(|x| x * w / sw).collect();
            for (y, row) in win.surface.pixels.chunks_exact_mut(sw).take(sh).enumerate() {
                let src = &pixels[(y * h / sh) * w..][..w];
                if sw == w {
                    for (o, p) in row.iter_mut().zip(src) {
                        *o = p & 0x00FF_FFFF;
                    }
                } else {
                    for (o, &sx) in row.iter_mut().zip(&xmap) {
                        *o = src[sx] & 0x00FF_FFFF;
                    }
                }
            }
        }
        win.needs_paint = false;
    }
}

// ---- vertex pipeline & drawing ----------------------------------------------------------------

#[derive(Clone, Debug)]
struct FvfLayout {
    xyzrhw: bool,
    has_xyz: bool,
    weights: u32,
    normal: bool,
    psize: bool,
    diffuse: bool,
    specular: bool,
    tex_count: u32,
    /// Floats per coordinate set.
    tex_floats: [u32; 8],
    stride: u32,
}

fn parse_fvf(fvf: u32) -> FvfLayout {
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
struct PipeVertex {
    clip: [f32; 4],
    color: [f32; 4],
    spec: [f32; 4],
    uv: [[f32; 2]; 2],
    /// Already screen-space (XYZRHW): x, y, z, rhw are in `clip`.
    screen: bool,
}

fn mat_mul_point(m: &[f32; 16], p: [f32; 4]) -> [f32; 4] {
    let mut o = [0.0; 4];
    for c in 0..4 {
        o[c] = p[0] * m[c] + p[1] * m[4 + c] + p[2] * m[8 + c] + p[3] * m[12 + c];
    }
    o
}

fn mat_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut o = [0.0; 16];
    for r in 0..4 {
        for c in 0..4 {
            o[r * 4 + c] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum();
        }
    }
    o
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l > 1e-12 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        v
    }
}

impl Device {
    fn rs_bool(&self, s: usize) -> bool {
        self.rs[s] != 0
    }

    /// The enabled lights, decoded from their D3DLIGHT8 bytes (once per draw, not per vertex).
    fn light_params(&self) -> Vec<LightParams> {
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

    fn lit_color(
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

    fn process(&self, e: &Emu, l: &FvfLayout, setup: &DrawSetup, addr: u32) -> Res<PipeVertex> {
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

    fn to_screen(&self, v: &PipeVertex) -> Vertex {
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
struct LightParams {
    directional: bool,
    diffuse: [f32; 3],
    ambient: [f32; 3],
    pos: [f32; 3],
    /// For directional lights: the unit vector toward the light.
    to_light: [f32; 3],
    range: f32,
    atten: [f32; 3],
}

/// What every vertex of one draw call shares.
struct DrawSetup {
    wvp: [f32; 16],
    lights: Vec<LightParams>,
}

fn lerp_pv(a: &PipeVertex, b: &PipeVertex, t: f32) -> PipeVertex {
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
fn clip_near(tri: [PipeVertex; 3]) -> ([PipeVertex; 4], usize) {
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

enum DrawCall {
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

fn index_count(ptype: u32, prims: u32) -> u32 {
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
fn triangle_count(ptype: u32, n: u32) -> u32 {
    match ptype {
        4 => n / 3,
        5 | 6 => n.saturating_sub(2),
        _ => 0,
    }
}

/// The vertex slots of triangle `i`.
fn triangle_slot(ptype: u32, i: u32) -> [u32; 3] {
    match ptype {
        4 => [3 * i, 3 * i + 1, 3 * i + 2],
        // alternate winding so every triangle faces the same way
        5 if i % 2 == 0 => [i, i + 1, i + 2],
        5 => [i + 1, i, i + 2],
        _ => [0, i + 1, i + 2],
    }
}

/// XP_D3D_PROBE=x,y: the pixel whose draws are described (debugging).
fn probe_point() -> Option<(f32, f32)> {
    static P: std::sync::OnceLock<Option<(f32, f32)>> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        let v = std::env::var("XP_D3D_PROBE").ok()?;
        let (x, y) = v.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    })
}

/// Does the screen-space triangle `t` cover pixel centre (px, py)?
fn covers(t: &[Vertex], px: f32, py: f32) -> bool {
    let e = |a: &Vertex, b: &Vertex| (b.x - a.x) * (py - a.y) - (b.y - a.y) * (px - a.x);
    let (w0, w1, w2) = (e(&t[1], &t[2]), e(&t[2], &t[0]), e(&t[0], &t[1]));
    (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0) || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0)
}

fn draw(e: &mut Emu, call: &DrawCall) -> Res<u32> {
    let Some(()) = with_device(e, |e, d| draw_inner(e, d, call))? else {
        return Ok(D3DERR_INVALIDCALL);
    };
    Ok(S_OK)
}

fn draw_inner(e: &mut Emu, d: &mut Device, call: &DrawCall) -> Res<()> {
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
    let fetch = |cache: &mut Vec<(u32, Projected, PipeVertex, Vertex)>, e: &Emu, d: &Device, slot: u32, corner: usize| -> Res<usize> {
        let vi = slot_to_vertex(e, slot)?;
        let key = (vi as usize).min(CACHE_LIMIT) + if vi as usize >= CACHE_LIMIT { corner } else { 0 };
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
        let scr = if kind == Projected::Behind { Vertex::default() } else { d.to_screen(&v) };
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
                let (ka, kb) = (fetch(cache, e, dev_view, a_slot, 0)?, fetch(cache, e, dev_view, b_slot, 1)?);
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
                d.frames, d.shader, scratch.verts.len(), &d.viewport[..4], rs.zenable, rs.zwrite, rs.zfunc,
                rs.alpha_blend, rs.src_blend, rs.dst_blend, rs.alpha_test, rs.alpha_func, rs.alpha_ref, rs.cull, rs.texture_factor,
                (stages[0].color_op, stages[0].color_arg1, stages[0].color_arg2, stages[0].alpha_op, stages[0].alpha_arg1, stages[0].alpha_arg2, stages[0].texcoord_index),
                gtex[0].as_ref().map(|(id, t)| (*id, t.format, t.levels.first().map(|l| (l.w, l.h)))),
                (stages[1].color_op, stages[1].color_arg1, stages[1].color_arg2, stages[1].alpha_op, stages[1].alpha_arg1, stages[1].alpha_arg2, stages[1].texcoord_index),
                gtex[1].as_ref().map(|(id, t)| (*id, t.format, t.levels.first().map(|l| (l.w, l.h)))),
                v.x, v.y, v.z, v.rhw, v.color, v.uv
            );
            eprintln!(
                "[probe]   lighting {} ambient {:#x} material {:.2?} colorvertex {} srcs diff/amb/emi {} {} {} lights {:?}",
                d.rs[137], d.rs[139], d.material, d.rs[141], d.rs[145], d.rs[147], d.rs[148],
                setup.lights.iter().map(|l| (l.directional, l.diffuse, l.ambient, l.to_light, l.range, l.atten)).collect::<Vec<_>>()
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
                eprintln!("[probe]   stage{s} texture {}x{} at {:#x}: mean {sum:.2?} max {max:.2?}", l.w, l.h, l.ptr);
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
fn thin_quad(out: &mut Vec<Vertex>, a: &Vertex, b: &Vertex) {
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

fn draw_line(t: &mut Target, a: &Vertex, b: &Vertex, rs: &RenderState) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader;
    use crate::tests::util::{Asm, CODE_VA, build_pe};

    #[test]
    fn vtables_have_the_documented_number_of_entries() {
        assert_eq!(D3D_METHODS.len(), 16, "IDirect3D8");
        assert_eq!(DEVICE_METHODS.len(), 97, "IDirect3DDevice8");
        assert_eq!(TEXTURE_METHODS.len(), 19, "IDirect3DTexture8");
        assert_eq!(SURFACE_METHODS.len(), 11, "IDirect3DSurface8");
        assert_eq!(VB_METHODS.len(), 14, "IDirect3DVertexBuffer8");
        assert_eq!(SWAPCHAIN_METHODS.len(), 5);
    }

    #[test]
    fn vtable_slots_match_the_header_order() {
        let slot = |table: &[(&str, usize)], name: &str| {
            table.iter().position(|(n, _)| *n == name).unwrap()
        };
        assert_eq!(slot(D3D_METHODS, "CreateDevice"), 15);
        assert_eq!(slot(D3D_METHODS, "GetAdapterIdentifier"), 5);
        assert_eq!(slot(DEVICE_METHODS, "Present"), 15);
        assert_eq!(slot(DEVICE_METHODS, "CreateTexture"), 20);
        assert_eq!(slot(DEVICE_METHODS, "CreateVertexBuffer"), 23);
        assert_eq!(slot(DEVICE_METHODS, "BeginScene"), 34);
        assert_eq!(slot(DEVICE_METHODS, "Clear"), 36);
        assert_eq!(slot(DEVICE_METHODS, "SetTransform"), 37);
        assert_eq!(slot(DEVICE_METHODS, "SetRenderState"), 50);
        assert_eq!(slot(DEVICE_METHODS, "SetTexture"), 61);
        assert_eq!(slot(DEVICE_METHODS, "SetTextureStageState"), 63);
        assert_eq!(slot(DEVICE_METHODS, "DrawPrimitive"), 70);
        assert_eq!(slot(DEVICE_METHODS, "DrawIndexedPrimitiveUP"), 73);
        assert_eq!(slot(DEVICE_METHODS, "SetVertexShader"), 76);
        assert_eq!(slot(DEVICE_METHODS, "SetStreamSource"), 83);
        assert_eq!(slot(DEVICE_METHODS, "SetIndices"), 85);
        assert_eq!(slot(DEVICE_METHODS, "DeletePatch"), 96);
        assert_eq!(slot(TEXTURE_METHODS, "LockRect"), 16);
        assert_eq!(slot(TEXTURE_METHODS, "GetSurfaceLevel"), 15);
        assert_eq!(slot(SURFACE_METHODS, "LockRect"), 9);
        assert_eq!(slot(VB_METHODS, "Lock"), 11);
    }

    #[test]
    fn every_method_resolves_with_its_argument_count() {
        for iface in ALL_IFACES {
            for (m, argc) in iface.methods() {
                let (got, _) = lookup("d3d8", &format!("{}::{}", iface.name(), m))
                    .unwrap_or_else(|| panic!("{}::{m}", iface.name()));
                assert_eq!(got, *argc, "{}::{m}", iface.name());
            }
        }
        assert!(lookup("d3d8", "Direct3DCreate8").is_some());
        assert!(lookup("d3d8", "IDirect3DDevice8::NoSuchThing").is_none());
        assert!(lookup("kernel32", "Direct3DCreate8").is_none());
    }

    #[test]
    fn fvf_layouts() {
        let tl = parse_fvf(0x004 | 0x040 | 0x100); // XYZRHW | DIFFUSE | TEX1
        assert!(tl.xyzrhw && tl.diffuse && tl.tex_count == 1);
        assert_eq!(tl.stride, 16 + 4 + 8);
        let lit = parse_fvf(0x002 | 0x010 | 0x200); // XYZ | NORMAL | TEX2
        assert_eq!(lit.stride, 12 + 12 + 16);
        let three_d_coords = parse_fvf(0x002 | 0x100 | (1 << 16)); // texcoord set 0 has 3 floats
        assert_eq!(three_d_coords.stride, 12 + 12);
        let blend = parse_fvf(0x006); // XYZB1
        assert_eq!(blend.stride, 16);
    }

    #[test]
    fn mip_chains() {
        assert_eq!(mip_chain(8, 4, 0), vec![(8, 4), (4, 2), (2, 1), (1, 1)]);
        assert_eq!(mip_chain(8, 8, 1), vec![(8, 8)]);
        assert_eq!(mip_chain(8, 8, 2), vec![(8, 8), (4, 4)]);
        assert_eq!(mip_chain(1, 1, 0), vec![(1, 1)]);
    }

    #[test]
    fn matrix_math_and_perspective_to_screen() {
        let mut t = identity();
        t[12] = 5.0; // translate x by 5
        assert_eq!(
            mat_mul_point(&t, [1.0, 2.0, 3.0, 1.0]),
            [6.0, 2.0, 3.0, 1.0]
        );
        let a = identity();
        assert_eq!(mat_mul(&a, &t), t);
        // a vertex at the clip-space centre lands in the middle of the viewport
        let dev = test_device(640, 480);
        let v = PipeVertex {
            clip: [0.0, 0.0, 0.5, 1.0],
            ..PipeVertex::default()
        };
        let s = dev.to_screen(&v);
        assert_eq!((s.x, s.y), (320.0, 240.0));
        let corner = dev.to_screen(&PipeVertex {
            clip: [-1.0, 1.0, 0.0, 1.0],
            ..PipeVertex::default()
        });
        assert_eq!(
            (corner.x, corner.y),
            (0.0, 0.0),
            "clip (-1, +1) is the top-left pixel"
        );
        let far = dev.to_screen(&PipeVertex {
            clip: [2.0, 0.0, 1.0, 2.0],
            ..PipeVertex::default()
        });
        assert_eq!(
            (far.x, far.rhw),
            (640.0, 0.5),
            "x/w = 1 is the right edge; 1/w is kept for perspective correction"
        );
    }

    #[test]
    fn near_plane_clipping_cuts_a_triangle() {
        let v = |x: f32, z: f32| PipeVertex {
            clip: [x, 0.0, z, 1.0],
            ..PipeVertex::default()
        };
        // one vertex behind the near plane: the triangle becomes a quad
        let (quad, len) = clip_near([v(0.0, 1.0), v(1.0, 1.0), v(0.0, -1.0)]);
        assert_eq!(len, 4);
        assert!(
            quad[..len].iter().all(|p| p.clip[2] >= -1e-6),
            "nothing left behind the plane"
        );
        // all behind: gone; all in front: untouched
        assert!(clip_near([v(0.0, -1.0), v(1.0, -1.0), v(0.0, -2.0)]).1 == 0);
        assert_eq!(clip_near([v(0.0, 1.0), v(1.0, 1.0), v(0.0, 2.0)]).1, 3);
    }

    #[test]
    fn primitive_assembly() {
        let triangle_slots = |p: u32, n: u32| {
            (0..triangle_count(p, n))
                .map(|i| triangle_slot(p, i))
                .collect::<Vec<_>>()
        };
        assert_eq!(triangle_slots(4, 6), vec![[0, 1, 2], [3, 4, 5]], "list");
        assert_eq!(
            triangle_slots(5, 5),
            vec![[0, 1, 2], [2, 1, 3], [2, 3, 4]],
            "strip alternates winding"
        );
        assert_eq!(
            triangle_slots(6, 5),
            vec![[0, 1, 2], [0, 2, 3], [0, 3, 4]],
            "fan"
        );
        assert_eq!(
            (
                index_count(4, 2),
                index_count(5, 2),
                index_count(2, 3),
                index_count(3, 3),
                index_count(1, 3)
            ),
            (6, 4, 6, 4, 3)
        );
    }

    fn test_device(w: u32, h: u32) -> Device {
        Device {
            hwnd: 0,
            params: Params::default(),
            back_surface: 0,
            depth_surface: 0,
            target: Target::new(w, h),
            rt: None,
            rs: default_render_states(),
            ts: default_stage_states(),
            world: identity(),
            view: identity(),
            proj: identity(),
            tex_transform: [identity(); 8],
            viewport: [0, 0, w, h, 0.0f32.to_bits(), 1.0f32.to_bits()],
            material: [0.0; 17],
            lights: HashMap::new(),
            textures: [0; 8],
            stream: (0, 0),
            indices: (0, 0),
            shader: 0,
            frames: 0,
            last_present: None,
            frame_times: Vec::new(),
            gpu: None,
            scene_open: false,
            recording: None,
            blocks: HashMap::new(),
            next_token: 1,
            scratch: DrawScratch::default(),
            pending_present: None,
        }
    }

    #[test]
    fn directional_lighting_follows_the_normal() {
        let mut d = test_device(4, 4);
        d.material = [
            1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0,
        ];
        let mut raw = [0u8; 104];
        raw[0..4].copy_from_slice(&3u32.to_le_bytes()); // directional
        for (i, v) in [1.0f32, 1.0, 1.0, 1.0].iter().enumerate() {
            raw[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_le_bytes()); // diffuse white
        }
        for (i, v) in [0.0f32, 0.0, -1.0].iter().enumerate() {
            raw[64 + 4 * i..68 + 4 * i].copy_from_slice(&v.to_le_bytes()); // light travels toward -Z
        }
        d.lights.insert(0, Light { raw, enabled: true });
        d.rs[145] = 0; // use the material, not vertex colours
        let lights = d.light_params();
        let facing = d.lit_color(&lights, [0.0; 3], [0.0, 0.0, 1.0], None);
        let away = d.lit_color(&lights, [0.0; 3], [0.0, 0.0, -1.0], None);
        let side = d.lit_color(&lights, [0.0; 3], [1.0, 0.0, 0.0], None);
        assert!(
            (facing[0] - 1.0).abs() < 1e-5,
            "a surface facing the light is fully lit"
        );
        assert_eq!(away[0], 0.0, "facing away: dark");
        assert_eq!(side[0], 0.0, "edge-on: dark");
        d.lights.get_mut(&0).unwrap().enabled = false;
        assert_eq!(
            d.lit_color(&d.light_params(), [0.0; 3], [0.0, 0.0, 1.0], None)[0],
            0.0,
            "disabled lights contribute nothing"
        );
    }

    // ---- guest-level: call everything through real vtables ----------------------------------

    const CALL: &[&str] = &["Direct3DCreate8"];
    const K32: &[&str] = &["ExitProcess"];

    /// Emit a COM call: push args (right to left), push `this` (in ESI), `call [vtbl + 4*slot]`.
    fn com_call(a: &mut Asm, obj_slot: u32, vt_slot: u32, args: &[u32]) {
        for v in args.iter().rev() {
            a.e(&[0x68]).e(&v.to_le_bytes());
        }
        a.e(&[0xFF, 0x35]).data_va(obj_slot); // push this
        a.e(&[0xA1]).data_va(obj_slot); // mov eax, this
        a.e(&[0x8B, 0x00]); // mov eax, [eax]  (vtable)
        a.e(&[0xFF, 0x90]).e(&(4 * vt_slot).to_le_bytes()); // call [eax + 4*slot]
    }

    /// A game-like program: Direct3DCreate8, CreateDevice, Clear to blue, draw one
    /// pre-transformed triangle from user memory, Present, ExitProcess(Present's result).
    fn render_program() -> Vec<u8> {
        build_pe(&[("d3d8.dll", CALL), ("kernel32.dll", K32)], |l| {
            let mut a = Asm::new();
            // params block at slot 0x10 (13 dwords): 64x48 windowed X8R8G8B8
            let params = crate::tests::util::data_va(0x10);
            a.e(&[0xC7, 0x05])
                .e(&crate::tests::util::data_va(0x10).to_le_bytes())
                .e(&64u32.to_le_bytes()); // width
            a.e(&[0xC7, 0x05])
                .e(&crate::tests::util::data_va(0x11).to_le_bytes())
                .e(&48u32.to_le_bytes()); // height
            a.e(&[0xC7, 0x05])
                .e(&crate::tests::util::data_va(0x12).to_le_bytes())
                .e(&FMT_X8R8G8B8.to_le_bytes());
            a.e(&[0xC7, 0x05])
                .e(&crate::tests::util::data_va(0x17).to_le_bytes())
                .e(&1u32.to_le_bytes()); // Windowed
            // pd3d = Direct3DCreate8(220)
            a.e(&[0x68]).e(&220u32.to_le_bytes()).call(l.iat_va(0, 0));
            a.store(0); // data[0] = IDirect3D8*
            // d3d->CreateDevice(0, HAL=1, hwnd=0, flags=0x20, &params, &data[1])
            com_call(
                &mut a,
                0,
                15,
                &[0, 1, 0, 0x20, params, crate::tests::util::data_va(1)],
            );
            a.store(2); // data[2] = HRESULT of CreateDevice
            // device->Clear(0, NULL, TARGET, 0xFF0000FF, 1.0f, 0)
            com_call(&mut a, 1, 36, &[0, 0, 1, 0xFF00_00FF, 1.0f32.to_bits(), 0]);
            // device->SetVertexShader(XYZRHW|DIFFUSE)
            com_call(&mut a, 1, 76, &[0x044]);
            // device->SetRenderState(CULLMODE=NONE), ZENABLE=0
            com_call(&mut a, 1, 50, &[22, 1]);
            com_call(&mut a, 1, 50, &[7, 0]);
            // DrawPrimitiveUP(TRIANGLELIST, 1, &verts, 20)
            com_call(
                &mut a,
                1,
                72,
                &[4, 1, crate::tests::util::data_va(0x40), 20],
            );
            // device->Present(0,0,0,0)
            com_call(&mut a, 1, 15, &[0, 0, 0, 0]);
            a.e(&[0x50]).call(l.iat_va(1, 0)); // ExitProcess(Present's HRESULT)
            a.finish()
        })
    }

    #[test]
    fn a_guest_program_can_create_a_device_clear_draw_and_present_through_vtables() {
        let mut emu = Emu::new();
        // The game's window: 64x48 client area.
        let hwnd = emu.wm.add_window(
            "GameWin",
            "game",
            crate::gui::wm::WS_VISIBLE,
            0,
            (0, 0, 64, 48),
            0,
            0,
        );
        emu.wm.window_mut(hwnd).unwrap().visible = true;
        loader::load(&mut emu, &render_program()).unwrap();
        // One triangle in screen space, red, covering the upper-left half: (x, y, z, rhw, diffuse)
        let verts: [(f32, f32, f32, f32, u32); 3] = [
            (0.0, 0.0, 0.5, 1.0, 0xFFFF0000),
            (64.0, 0.0, 0.5, 1.0, 0xFFFF0000),
            (0.0, 48.0, 0.5, 1.0, 0xFFFF0000),
        ];
        // device params need hDeviceWindow: slot 0x16
        emu.mem
            .write(crate::tests::util::data_va(0x16), 4, hwnd)
            .unwrap();
        for (i, v) in verts.iter().enumerate() {
            let base = crate::tests::util::data_va(0x40) + 20 * i as u32;
            for (k, f) in [v.0, v.1, v.2, v.3].iter().enumerate() {
                emu.mem.write(base + 4 * k as u32, 4, f.to_bits()).unwrap();
            }
            emu.mem.write(base + 16, 4, v.4).unwrap();
        }
        let stop = emu.run(2_000_000);
        assert_eq!(stop, crate::cpu::Stop::Exit(0), "every call returned S_OK");
        assert_eq!(
            emu.mem.read(crate::tests::util::data_va(2), 4).unwrap(),
            0,
            "CreateDevice succeeded"
        );
        assert_eq!(emu.win.d3d.frames(), 1);
        if let Some(g) = emu.win.d3d.dev.as_ref().and_then(|d| d.gpu.as_ref()) {
            assert_eq!(
                g.draws, 1,
                "the triangle went through Metal, not the software rasterizer"
            );
        }

        finish_present(&mut emu, true);
        let (w, h, px) = emu.win.d3d.frame().unwrap();
        assert_eq!((w, h), (64, 48));
        assert_eq!(
            px[(5 * 64 + 5) as usize],
            0xFFFF0000,
            "inside the triangle: red"
        );
        assert_eq!(
            px[(40 * 64 + 60) as usize],
            0xFF0000FF,
            "outside: the blue clear colour"
        );

        // ...and Present put it on the game's window.
        let surface = &emu.wm.window(hwnd).unwrap().surface;
        assert_eq!(surface.get_pixel(5, 5), Some(0xFF0000));
        assert_eq!(surface.get_pixel(60, 40), Some(0x0000FF));
        let _ = CODE_VA;
    }
}
