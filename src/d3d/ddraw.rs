//! DirectDraw (DirectX 1–6 era 2D), emulated.
//!
//! The objects are COM objects as in `d3d8`: a guest block whose first word points at a
//! vtable of HLE stubs. Surfaces are plain guest memory the game locks and writes; whatever
//! reaches the primary surface (an unlock, a blit, a flip, a palette change) is converted
//! to RGB, through the palette in 8-bit modes, and copied into the game's window.

use std::collections::HashMap;

use crate::cpu::{Emu, Res};
use crate::win32::{Args, Handler};

const DD_OK: u32 = 0;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DDERR_UNSUPPORTED: u32 = 0x8000_4001;
const DDERR_INVALIDPARAMS: u32 = 0x8007_0057;
const DDERR_NOTFOUND: u32 = 0x8876_00FF;
const DDERR_NOPALETTEATTACHED: u32 = 0x8876_0230;
const DDERR_NOCLIPPERATTACHED: u32 = 0x8876_0226;
const DDERR_NOCOLORKEY: u32 = 0x8876_00D7;

// DDSURFACEDESC
const SD_FLAGS: u32 = 4;
const SD_HEIGHT: u32 = 8;
const SD_WIDTH: u32 = 12;
const SD_PITCH: u32 = 16;
const SD_BACKBUFFERS: u32 = 20;
const SD_SURFACE: u32 = 36;
const SD_CKSRCBLT: u32 = 64;
const SD_PIXELFORMAT: u32 = 72;
const SD_CAPS: u32 = 104;
const DDSD_CAPS: u32 = 1;
const DDSD_HEIGHT: u32 = 2;
const DDSD_WIDTH: u32 = 4;
const DDSD_PITCH: u32 = 8;
const DDSD_BACKBUFFERCOUNT: u32 = 0x20;
const DDSD_LPSURFACE: u32 = 0x800;
const DDSD_PIXELFORMAT: u32 = 0x1000;
const DDSD_CKSRCBLT: u32 = 0x10000;
const DDSD_REFRESHRATE: u32 = 0x20000;

const DDSCAPS_BACKBUFFER: u32 = 4;
const DDSCAPS_COMPLEX: u32 = 8;
const DDSCAPS_FLIP: u32 = 0x10;
const DDSCAPS_FRONTBUFFER: u32 = 0x20;
const DDSCAPS_OFFSCREENPLAIN: u32 = 0x40;
const DDSCAPS_PRIMARYSURFACE: u32 = 0x200;
const DDSCAPS_SYSTEMMEMORY: u32 = 0x800;
const DDSCAPS_VIDEOMEMORY: u32 = 0x4000;
const DDSCAPS_VISIBLE: u32 = 0x8000;

const DDPF_PALETTEINDEXED8: u32 = 0x20;
const DDPF_RGB: u32 = 0x40;

const DDBLT_COLORFILL: u32 = 0x400;
const DDBLT_KEYSRC: u32 = 0x8000;
const DDBLTFAST_SRCCOLORKEY: u32 = 1;
const DDCKEY_SRCBLT: u32 = 8;

// ---- interfaces ------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Iface {
    DirectDraw,
    DirectDraw2,
    Surface,
    Palette,
    Clipper,
}

const ALL_IFACES: [Iface; 5] = [Iface::DirectDraw, Iface::DirectDraw2, Iface::Surface, Iface::Palette, Iface::Clipper];

impl Iface {
    fn name(self) -> &'static str {
        match self {
            Iface::DirectDraw => "IDirectDraw",
            Iface::DirectDraw2 => "IDirectDraw2",
            Iface::Surface => "IDirectDrawSurface",
            Iface::Palette => "IDirectDrawPalette",
            Iface::Clipper => "IDirectDrawClipper",
        }
    }

    /// Methods in vtable order, with their argument counts including `this`.
    fn methods(self) -> &'static [(&'static str, usize)] {
        match self {
            Iface::DirectDraw => DD_METHODS,
            Iface::DirectDraw2 => DD2_METHODS,
            Iface::Surface => SURFACE_METHODS,
            Iface::Palette => PALETTE_METHODS,
            Iface::Clipper => CLIPPER_METHODS,
        }
    }
}

const DD_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("Compact", 1),
    ("CreateClipper", 4),
    ("CreatePalette", 5),
    ("CreateSurface", 4),
    ("DuplicateSurface", 3),
    ("EnumDisplayModes", 5),
    ("EnumSurfaces", 5),
    ("FlipToGDISurface", 1),
    ("GetCaps", 3),
    ("GetDisplayMode", 2),
    ("GetFourCCCodes", 3),
    ("GetGDISurface", 2),
    ("GetMonitorFrequency", 2),
    ("GetScanLine", 2),
    ("GetVerticalBlankStatus", 2),
    ("Initialize", 2),
    ("RestoreDisplayMode", 1),
    ("SetCooperativeLevel", 3),
    ("SetDisplayMode", 4),
    ("WaitForVerticalBlank", 3),
];

/// IDirectDraw2: SetDisplayMode gains a refresh rate and flags, and one method is added.
const DD2_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("Compact", 1),
    ("CreateClipper", 4),
    ("CreatePalette", 5),
    ("CreateSurface", 4),
    ("DuplicateSurface", 3),
    ("EnumDisplayModes", 5),
    ("EnumSurfaces", 5),
    ("FlipToGDISurface", 1),
    ("GetCaps", 3),
    ("GetDisplayMode", 2),
    ("GetFourCCCodes", 3),
    ("GetGDISurface", 2),
    ("GetMonitorFrequency", 2),
    ("GetScanLine", 2),
    ("GetVerticalBlankStatus", 2),
    ("Initialize", 2),
    ("RestoreDisplayMode", 1),
    ("SetCooperativeLevel", 3),
    ("SetDisplayMode", 6),
    ("WaitForVerticalBlank", 3),
    ("GetAvailableVidMem", 4),
];

/// IDirectDrawSurface3; versions 1 and 2 are prefixes of it, so one vtable serves all three.
const SURFACE_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("AddAttachedSurface", 2),
    ("AddOverlayDirtyRect", 2),
    ("Blt", 6),
    ("BltBatch", 4),
    ("BltFast", 6),
    ("DeleteAttachedSurface", 3),
    ("EnumAttachedSurfaces", 3),
    ("EnumOverlayZOrders", 4),
    ("Flip", 3),
    ("GetAttachedSurface", 3),
    ("GetBltStatus", 2),
    ("GetCaps", 2),
    ("GetClipper", 2),
    ("GetColorKey", 3),
    ("GetDC", 2),
    ("GetFlipStatus", 2),
    ("GetOverlayPosition", 3),
    ("GetPalette", 2),
    ("GetPixelFormat", 2),
    ("GetSurfaceDesc", 2),
    ("Initialize", 3),
    ("IsLost", 1),
    ("Lock", 5),
    ("ReleaseDC", 2),
    ("Restore", 1),
    ("SetClipper", 2),
    ("SetColorKey", 3),
    ("SetOverlayPosition", 3),
    ("SetPalette", 2),
    ("Unlock", 2),
    ("UpdateOverlay", 6),
    ("UpdateOverlayDisplay", 2),
    ("UpdateOverlayZOrder", 3),
    ("GetDDInterface", 2),
    ("PageLock", 2),
    ("PageUnlock", 2),
    ("SetSurfaceDesc", 3),
];

const PALETTE_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("GetCaps", 2),
    ("GetEntries", 5),
    ("Initialize", 4),
    ("SetEntries", 5),
];

const CLIPPER_METHODS: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("GetClipList", 4),
    ("GetHWnd", 2),
    ("Initialize", 3),
    ("IsClipListChanged", 2),
    ("SetClipList", 3),
    ("SetHWnd", 3),
];

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]]
}

const IID_IUNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
const IID_DIRECTDRAW: [u8; 16] = guid(0x6C14DB80, 0xA733, 0x11CE, [0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60]);
const IID_DIRECTDRAW2: [u8; 16] = guid(0xB3A6F3E0, 0x2B43, 0x11CF, [0xA2, 0xDE, 0x00, 0xAA, 0x00, 0xB9, 0x33, 0x56]);
const IID_SURFACES: [[u8; 16]; 3] = [
    guid(0x6C14DB81, 0xA733, 0x11CE, [0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60]),
    guid(0x57805885, 0x6EEC, 0x11CF, [0x94, 0x41, 0xA8, 0x23, 0x03, 0xC1, 0x0E, 0x27]),
    guid(0xDA044E00, 0x69B2, 0x11D0, [0xA1, 0xD5, 0x00, 0xAA, 0x00, 0xB8, 0xDF, 0xBB]),
];

// ---- objects ---------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Format {
    bpp: u32,
    masks: [u32; 3],
}

impl Format {
    fn of_bpp(bpp: u32) -> Format {
        let masks = match bpp {
            16 => [0xF800, 0x07E0, 0x001F],
            24 | 32 => [0xFF_0000, 0xFF00, 0xFF],
            _ => [0; 3],
        };
        Format { bpp, masks }
    }
}

struct Surface {
    refs: u32,
    caps: u32,
    w: u32,
    h: u32,
    pitch: u32,
    ptr: u32,
    format: Format,
    /// The next surface in a flip chain (a primary's back buffer).
    back: u32,
    palette: u32,
    clipper: u32,
    color_key: Option<u32>,
}

enum Obj {
    DirectDraw { refs: u32 },
    Surface(Surface),
    /// Entries as 0x00RRGGBB.
    Palette { refs: u32, flags: u32, entries: Box<[u32; 256]> },
    Clipper { refs: u32, hwnd: u32 },
}

impl Obj {
    fn refs(&mut self) -> &mut u32 {
        match self {
            Obj::DirectDraw { refs } | Obj::Palette { refs, .. } | Obj::Clipper { refs, .. } => refs,
            Obj::Surface(s) => &mut s.refs,
        }
    }
}

pub struct DDraw {
    objects: HashMap<u32, Obj>,
    vtables: HashMap<&'static str, u32>,
    /// Display mode: width, height and bits per pixel.
    mode: (u32, u32, u32),
    hwnd: u32,
    /// The program has the screen to itself (DDSCL_FULLSCREEN) and has set a mode: the
    /// screen is then the mode's size.
    fullscreen: bool,
    mode_set: bool,
    primary: u32,
    /// Strings handed to DirectDrawEnumerate's callback.
    names: u32,
    pub frames: u64,
}

impl DDraw {
    /// The screen size while a full-screen program has changed the display mode.
    pub fn screen(&self) -> Option<(u32, u32)> {
        (self.fullscreen && self.mode_set).then_some((self.mode.0, self.mode.1))
    }
}

impl Default for DDraw {
    fn default() -> DDraw {
        DDraw { objects: HashMap::new(), vtables: HashMap::new(), mode: (1024, 768, 32), hwnd: 0, fullscreen: false, mode_set: false, primary: 0, names: 0, frames: 0 }
    }
}

fn vtable(e: &mut Emu, iface: Iface) -> Res<u32> {
    if let Some(&v) = e.win.ddraw.vtables.get(iface.name()) {
        return Ok(v);
    }
    let methods = iface.methods();
    let base = e.heap_alloc(4 * methods.len() as u32)?;
    for (i, (m, _)) in methods.iter().enumerate() {
        let stub = e.win.resolve("ddraw", &format!("{}::{}", iface.name(), m));
        e.mem.write(base + 4 * i as u32, 4, stub)?;
    }
    e.win.ddraw.vtables.insert(iface.name(), base);
    Ok(base)
}

fn new_object(e: &mut Emu, iface: Iface, obj: Obj) -> Res<u32> {
    let vt = vtable(e, iface)?;
    let ptr = e.heap_alloc(16)?;
    e.mem.write(ptr, 4, vt)?;
    e.win.ddraw.objects.insert(ptr, obj);
    Ok(ptr)
}

fn add_ref(e: &mut Emu, obj: u32) -> u32 {
    e.win.ddraw.objects.get_mut(&obj).map_or(1, |o| {
        *o.refs() += 1;
        *o.refs()
    })
}

fn surface(e: &Emu, p: u32) -> Option<&Surface> {
    match e.win.ddraw.objects.get(&p) {
        Some(Obj::Surface(s)) => Some(s),
        _ => None,
    }
}

fn surface_mut(e: &mut Emu, p: u32) -> Option<&mut Surface> {
    match e.win.ddraw.objects.get_mut(&p) {
        Some(Obj::Surface(s)) => Some(s),
        _ => None,
    }
}

fn write_out(e: &mut Emu, at: u32, v: u32) -> Res<()> {
    if at != 0 {
        e.mem.write(at, 4, v)?;
    }
    Ok(())
}

// ---- entry points ----------------------------------------------------------------------------

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "ddraw" {
        return None;
    }
    match name {
        "DirectDrawCreate" => return Some((3, direct_draw_create)),
        "DirectDrawEnumerateA" => return Some((2, direct_draw_enumerate)),
        _ => {}
    }
    let (iface_name, method) = name.split_once("::")?;
    let iface = ALL_IFACES.into_iter().find(|i| i.name() == iface_name)?;
    let (_, argc) = *iface.methods().iter().find(|(m, _)| *m == method)?;
    Some((argc, handler_for(iface, method)))
}

fn direct_draw_create(e: &mut Emu, a: &Args) -> Res<u32> {
    let dd = new_object(e, Iface::DirectDraw, Obj::DirectDraw { refs: 1 })?;
    e.mem.write(a[1], 4, dd)?;
    Ok(DD_OK)
}

/// One display driver: the primary display, identified by a null GUID.
fn direct_draw_enumerate(e: &mut Emu, a: &Args) -> Res<u32> {
    if e.win.ddraw.names == 0 {
        let p = e.heap_alloc(64)?;
        e.mem.write_bytes(p, b"Primary Display Driver\0display\0")?;
        e.win.ddraw.names = p;
    }
    let names = e.win.ddraw.names;
    e.call_function(a[0], &[0, names, names + 23, a[1]])?;
    Ok(DD_OK)
}

/// Methods nobody has needed yet succeed quietly; the trace shows the call.
fn ok(_: &mut Emu, _: &Args) -> Res<u32> {
    Ok(DD_OK)
}

fn handler_for(iface: Iface, method: &str) -> Handler {
    match method {
        "QueryInterface" => query_interface,
        "AddRef" => |e, a| Ok(add_ref(e, a[0])),
        "Release" => release,
        _ => match iface {
            Iface::DirectDraw | Iface::DirectDraw2 => dd_method(method),
            Iface::Surface => surface_method(method),
            Iface::Palette => palette_method(method),
            Iface::Clipper => clipper_method(method),
        },
    }
}

fn query_interface(e: &mut Emu, a: &Args) -> Res<u32> {
    let want: [u8; 16] = e.mem.read_bytes(a[1], 16)?.try_into().unwrap();
    let answer = match e.win.ddraw.objects.get(&a[0]) {
        Some(Obj::DirectDraw { .. }) if want == IID_DIRECTDRAW2 => {
            Some(new_object(e, Iface::DirectDraw2, Obj::DirectDraw { refs: 1 })?)
        }
        Some(Obj::DirectDraw { .. }) if want == IID_DIRECTDRAW => {
            Some(new_object(e, Iface::DirectDraw, Obj::DirectDraw { refs: 1 })?)
        }
        Some(Obj::Surface(_)) if IID_SURFACES.contains(&want) => {
            add_ref(e, a[0]);
            Some(a[0])
        }
        Some(_) if want == IID_IUNKNOWN => {
            add_ref(e, a[0]);
            Some(a[0])
        }
        _ => None,
    };
    match answer {
        Some(p) => {
            e.mem.write(a[2], 4, p)?;
            Ok(DD_OK)
        }
        None => {
            if e.win.trace {
                eprintln!("[ddraw] QueryInterface for an unsupported interface {want:02x?}");
            }
            e.mem.write(a[2], 4, 0)?;
            Ok(E_NOINTERFACE)
        }
    }
}

fn release(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(o) = e.win.ddraw.objects.get_mut(&a[0]) else { return Ok(0) };
    let refs = o.refs();
    *refs = refs.saturating_sub(1);
    let left = *refs;
    if left == 0 {
        let mut chain = Vec::new();
        let mut at = a[0];
        while let Some(Obj::Surface(s)) = e.win.ddraw.objects.remove(&at) {
            chain.push(s.ptr);
            at = s.back;
            if at == a[0] {
                break;
            }
        }
        for p in chain {
            e.free_virtual(p);
        }
        if e.win.ddraw.primary == a[0] {
            e.win.ddraw.primary = 0;
        }
        e.heap_free(a[0]);
    }
    Ok(left)
}

// ---- IDirectDraw -----------------------------------------------------------------------------

/// The modes offered: the usual 4:3 sizes in 8, 16 and 32 bits.
const MODES: &[(u32, u32)] = &[(640, 480), (800, 600), (1024, 768)];

fn dd_method(method: &str) -> Handler {
    match method {
        "SetCooperativeLevel" => |e, a| {
            e.win.ddraw.hwnd = a[1];
            e.win.ddraw.fullscreen = a[2] & 1 != 0; // DDSCL_FULLSCREEN
            Ok(DD_OK)
        },
        "SetDisplayMode" => |e, a| {
            let dd = &mut e.win.ddraw;
            dd.mode = (a[1], a[2], a[3]);
            dd.mode_set = true;
            // The screen changes size, and the program's full-screen window with it.
            if dd.fullscreen && dd.hwnd != 0 {
                let hwnd = dd.hwnd;
                crate::gui::user32::move_window(e, hwnd, 0, 0, a[1] as i32, a[2] as i32)?;
            }
            Ok(DD_OK)
        },
        "RestoreDisplayMode" => |e, _| {
            e.win.ddraw.mode_set = false;
            Ok(DD_OK)
        },
        "GetDisplayMode" => |e, a| {
            let (w, h, bpp) = e.win.ddraw.mode;
            write_desc(e, a[1], w, h, w * bpp / 8, Format::of_bpp(bpp), DDSCAPS_PRIMARYSURFACE, 0, None)?;
            Ok(DD_OK)
        },
        "EnumDisplayModes" => |e, a| {
            let desc = e.heap_alloc(108)?;
            'modes: for &(w, h) in MODES {
                for bpp in [8, 16, 32] {
                    write_desc(e, desc, w, h, w * bpp / 8, Format::of_bpp(bpp), 0, 0, None)?;
                    if e.call_function(a[4], &[desc, a[3]])? == 0 {
                        break 'modes;
                    }
                }
            }
            e.heap_free(desc);
            Ok(DD_OK)
        },
        "GetCaps" => |e, a| {
            for caps in [a[1], a[2]] {
                if caps == 0 {
                    continue;
                }
                let size = e.mem.read(caps, 4)?.clamp(4, 0x180);
                e.mem.write_bytes(caps + 4, &vec![0; size as usize - 4])?;
                // BLT, BLTSTRETCH, COLORKEY, PALETTE, BLTCOLORFILL
                e.mem.write(caps + 4, 4, 0x40 | 0x200 | 0x400 | 0x0080_0000 | 0x0400_0000)?;
                e.mem.write(caps + 24, 4, 4)?; // DDPCAPS_8BIT
                if size > 68 {
                    e.mem.write(caps + 60, 4, 32 << 20)?; // video memory total, free
                    e.mem.write(caps + 64, 4, 32 << 20)?;
                }
            }
            Ok(DD_OK)
        },
        "GetAvailableVidMem" => |e, a| {
            write_out(e, a[2], 32 << 20)?;
            write_out(e, a[3], 32 << 20)?;
            Ok(DD_OK)
        },
        "GetMonitorFrequency" => |e, a| {
            write_out(e, a[1], 60)?;
            Ok(DD_OK)
        },
        "GetVerticalBlankStatus" => |e, a| {
            write_out(e, a[1], 1)?;
            Ok(DD_OK)
        },
        "GetScanLine" => |e, a| {
            write_out(e, a[1], 0)?;
            Ok(DD_OK)
        },
        "CreateSurface" => create_surface,
        "CreatePalette" => |e, a| {
            let mut entries = Box::new([0u32; 256]);
            let n = if a[1] & 4 != 0 { 16 } else { 256 }; // DDPCAPS_4BIT
            if a[2] != 0 {
                for (i, slot) in entries.iter_mut().enumerate().take(n) {
                    *slot = palette_entry(e.mem.read(a[2] + 4 * i as u32, 4)?);
                }
            }
            let p = new_object(e, Iface::Palette, Obj::Palette { refs: 1, flags: a[1], entries })?;
            e.mem.write(a[3], 4, p)?;
            Ok(DD_OK)
        },
        "CreateClipper" => |e, a| {
            let p = new_object(e, Iface::Clipper, Obj::Clipper { refs: 1, hwnd: 0 })?;
            e.mem.write(a[2], 4, p)?;
            Ok(DD_OK)
        },
        "GetGDISurface" => |e, a| {
            let p = e.win.ddraw.primary;
            if p == 0 {
                return Ok(DDERR_NOTFOUND);
            }
            add_ref(e, p);
            e.mem.write(a[1], 4, p)?;
            Ok(DD_OK)
        },
        "GetFourCCCodes" => |e, a| {
            write_out(e, a[1], 0)?;
            Ok(DD_OK)
        },
        _ => ok,
    }
}

/// PALETTEENTRY (r, g, b, flags bytes) as 0x00RRGGBB.
fn palette_entry(v: u32) -> u32 {
    (v & 0xFF) << 16 | (v & 0xFF00) | (v >> 16) & 0xFF
}

/// Fill a DDSURFACEDESC (the version-1 layout, which DDSURFACEDESC2 extends).
#[allow(clippy::too_many_arguments)]
fn write_desc(e: &mut Emu, d: u32, w: u32, h: u32, pitch: u32, f: Format, caps: u32, ptr: u32, key: Option<u32>) -> Res<()> {
    let size = e.mem.read(d, 4)?;
    let size = if size == 124 { 124 } else { 108 };
    e.mem.write(d, 4, size)?;
    e.mem.write_bytes(d + 4, &vec![0; size as usize - 4])?;
    let mut flags = DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PITCH | DDSD_PIXELFORMAT | DDSD_REFRESHRATE;
    e.mem.write(d + SD_HEIGHT, 4, h)?;
    e.mem.write(d + SD_WIDTH, 4, w)?;
    e.mem.write(d + SD_PITCH, 4, pitch)?;
    e.mem.write(d + 24, 4, 60)?; // refresh rate
    if ptr != 0 {
        flags |= DDSD_LPSURFACE;
        e.mem.write(d + SD_SURFACE, 4, ptr)?;
    }
    if let Some(k) = key {
        flags |= DDSD_CKSRCBLT;
        e.mem.write(d + SD_CKSRCBLT, 4, k)?;
        e.mem.write(d + SD_CKSRCBLT + 4, 4, k)?;
    }
    let pf = d + SD_PIXELFORMAT;
    e.mem.write(pf, 4, 32)?;
    if f.bpp == 8 {
        e.mem.write(pf + 4, 4, DDPF_PALETTEINDEXED8 | DDPF_RGB)?;
    } else {
        e.mem.write(pf + 4, 4, DDPF_RGB)?;
        for (i, m) in f.masks.iter().enumerate() {
            e.mem.write(pf + 16 + 4 * i as u32, 4, *m)?;
        }
    }
    e.mem.write(pf + 12, 4, f.bpp)?;
    e.mem.write(d + SD_CAPS, 4, caps)?;
    e.mem.write(d + SD_FLAGS, 4, flags)?;
    Ok(())
}

fn alloc_surface(e: &mut Emu, w: u32, h: u32, format: Format, caps: u32) -> Res<u32> {
    let pitch = (w * format.bpp.div_ceil(8) + 3) & !3;
    let ptr = e.alloc_virtual((pitch * h).max(1));
    let s = Surface { refs: 1, caps, w, h, pitch, ptr, format, back: 0, palette: 0, clipper: 0, color_key: None };
    new_object(e, Iface::Surface, Obj::Surface(s))
}

fn create_surface(e: &mut Emu, a: &Args) -> Res<u32> {
    let d = a[1];
    let flags = e.mem.read(d + SD_FLAGS, 4)?;
    let caps = if flags & DDSD_CAPS != 0 { e.mem.read(d + SD_CAPS, 4)? } else { 0 };
    let (mw, mh, mbpp) = e.win.ddraw.mode;
    let format = if flags & DDSD_PIXELFORMAT != 0 {
        let pf = d + SD_PIXELFORMAT;
        let pff = e.mem.read(pf + 4, 4)?;
        let bpp = if pff & DDPF_PALETTEINDEXED8 != 0 { 8 } else { e.mem.read(pf + 12, 4)? };
        let mut f = Format::of_bpp(bpp);
        if pff & DDPF_RGB != 0 && bpp > 8 {
            f.masks = [e.mem.read(pf + 16, 4)?, e.mem.read(pf + 20, 4)?, e.mem.read(pf + 24, 4)?];
        }
        f
    } else {
        Format::of_bpp(mbpp)
    };
    let p = if caps & DDSCAPS_PRIMARYSURFACE != 0 {
        let caps = caps | DDSCAPS_VISIBLE | DDSCAPS_VIDEOMEMORY | DDSCAPS_FRONTBUFFER;
        let p = alloc_surface(e, mw, mh, Format::of_bpp(mbpp), caps)?;
        e.win.ddraw.primary = p;
        if caps & DDSCAPS_FLIP != 0 {
            let n = if flags & DDSD_BACKBUFFERCOUNT != 0 { e.mem.read(d + SD_BACKBUFFERS, 4)?.max(1) } else { 1 };
            // The chain is circular: primary -> back 1 -> ... -> back n -> primary.
            let mut prev = p;
            for i in 0..n {
                let bcaps = DDSCAPS_FLIP | DDSCAPS_COMPLEX | DDSCAPS_VIDEOMEMORY | if i == 0 { DDSCAPS_BACKBUFFER } else { 0 };
                let b = alloc_surface(e, mw, mh, Format::of_bpp(mbpp), bcaps)?;
                surface_mut(e, prev).unwrap().back = b;
                prev = b;
            }
            surface_mut(e, prev).unwrap().back = p;
        }
        p
    } else {
        if flags & (DDSD_WIDTH | DDSD_HEIGHT) != DDSD_WIDTH | DDSD_HEIGHT {
            return Ok(DDERR_INVALIDPARAMS);
        }
        let (w, h) = (e.mem.read(d + SD_WIDTH, 4)?, e.mem.read(d + SD_HEIGHT, 4)?);
        let mem = if caps & DDSCAPS_SYSTEMMEMORY != 0 { DDSCAPS_SYSTEMMEMORY } else { DDSCAPS_VIDEOMEMORY };
        let p = alloc_surface(e, w, h, format, caps | mem | if caps & 0x7FF == 0 { DDSCAPS_OFFSCREENPLAIN } else { 0 })?;
        if flags & DDSD_CKSRCBLT != 0 {
            let k = e.mem.read(d + SD_CKSRCBLT, 4)?;
            surface_mut(e, p).unwrap().color_key = Some(k);
        }
        p
    };
    e.mem.write(a[2], 4, p)?;
    Ok(DD_OK)
}

// ---- IDirectDrawSurface ----------------------------------------------------------------------

fn surface_method(method: &str) -> Handler {
    match method {
        "Lock" => |e, a| {
            let Some(s) = surface(e, a[0]) else { return Ok(DDERR_INVALIDPARAMS) };
            let (w, h, pitch, f, caps, key) = (s.w, s.h, s.pitch, s.format, s.caps, s.color_key);
            let mut ptr = s.ptr;
            if a[1] != 0 {
                let (x, y) = (e.mem.read(a[1], 4)?, e.mem.read(a[1] + 4, 4)?);
                ptr += y * pitch + x * f.bpp.div_ceil(8);
            }
            write_desc(e, a[2], w, h, pitch, f, caps, ptr, key)?;
            Ok(DD_OK)
        },
        "Unlock" => |e, a| {
            show_if_primary(e, a[0])?;
            Ok(DD_OK)
        },
        "GetSurfaceDesc" => |e, a| {
            let Some(s) = surface(e, a[0]) else { return Ok(DDERR_INVALIDPARAMS) };
            let (w, h, pitch, f, caps, key) = (s.w, s.h, s.pitch, s.format, s.caps, s.color_key);
            write_desc(e, a[1], w, h, pitch, f, caps, 0, key)?;
            Ok(DD_OK)
        },
        "GetCaps" => |e, a| {
            let caps = surface(e, a[0]).map_or(0, |s| s.caps);
            e.mem.write(a[1], 4, caps)?;
            Ok(DD_OK)
        },
        "GetPixelFormat" => |e, a| {
            let Some(s) = surface(e, a[0]) else { return Ok(DDERR_INVALIDPARAMS) };
            let f = s.format;
            // Reuse the descriptor writer on scratch memory, then copy its pixel format out.
            let scratch = e.heap_alloc(108)?;
            e.mem.write(scratch, 4, 108)?;
            write_desc(e, scratch, 1, 1, 4, f, 0, 0, None)?;
            let pf = e.mem.read_bytes(scratch + SD_PIXELFORMAT, 32)?;
            e.mem.write_bytes(a[1], &pf)?;
            e.heap_free(scratch);
            Ok(DD_OK)
        },
        "GetAttachedSurface" => |e, a| {
            let want = e.mem.read(a[1], 4)?;
            let back = surface(e, a[0]).map_or(0, |s| s.back);
            let found = back != 0 && surface(e, back).is_some_and(|b| b.caps & want == want);
            if !found {
                return Ok(DDERR_NOTFOUND);
            }
            add_ref(e, back);
            e.mem.write(a[2], 4, back)?;
            Ok(DD_OK)
        },
        "Flip" => flip,
        "Blt" => blt,
        "BltFast" => blt_fast,
        "SetPalette" => |e, a| {
            if a[1] != 0 {
                add_ref(e, a[1]);
            }
            let Some(s) = surface_mut(e, a[0]) else { return Ok(DDERR_INVALIDPARAMS) };
            s.palette = a[1];
            show_if_primary(e, a[0])?;
            Ok(DD_OK)
        },
        "GetPalette" => |e, a| {
            let p = surface(e, a[0]).map_or(0, |s| s.palette);
            if p == 0 {
                return Ok(DDERR_NOPALETTEATTACHED);
            }
            add_ref(e, p);
            e.mem.write(a[1], 4, p)?;
            Ok(DD_OK)
        },
        "SetClipper" => |e, a| {
            if let Some(s) = surface_mut(e, a[0]) {
                s.clipper = a[1];
            }
            Ok(DD_OK)
        },
        "GetClipper" => |e, a| {
            let c = surface(e, a[0]).map_or(0, |s| s.clipper);
            if c == 0 {
                return Ok(DDERR_NOCLIPPERATTACHED);
            }
            add_ref(e, c);
            e.mem.write(a[1], 4, c)?;
            Ok(DD_OK)
        },
        "SetColorKey" => |e, a| {
            let key = if a[2] != 0 { Some(e.mem.read(a[2], 4)?) } else { None };
            if a[1] & DDCKEY_SRCBLT != 0
                && let Some(s) = surface_mut(e, a[0]) {
                    s.color_key = key;
                }
            Ok(DD_OK)
        },
        "GetColorKey" => |e, a| {
            match surface(e, a[0]).and_then(|s| s.color_key) {
                Some(k) if a[1] & DDCKEY_SRCBLT != 0 => {
                    e.mem.write(a[2], 4, k)?;
                    e.mem.write(a[2] + 4, 4, k)?;
                    Ok(DD_OK)
                }
                _ => Ok(DDERR_NOCOLORKEY),
            }
        },
        "GetDC" | "ReleaseDC" => |_, _| Ok(DDERR_UNSUPPORTED),
        "IsLost" | "Restore" | "GetBltStatus" | "GetFlipStatus" | "PageLock" | "PageUnlock" => ok,
        _ => ok,
    }
}

fn flip(e: &mut Emu, a: &Args) -> Res<u32> {
    // Rotate the memory along the chain, so the front surface shows what the back one held.
    let mut chain = vec![a[0]];
    loop {
        let next = surface(e, *chain.last().unwrap()).map_or(0, |s| s.back);
        if next == 0 || next == a[0] {
            break;
        }
        chain.push(next);
    }
    if chain.len() < 2 {
        return Ok(DD_OK);
    }
    let ptrs: Vec<u32> = chain.iter().map(|&s| surface(e, s).unwrap().ptr).collect();
    for (i, &s) in chain.iter().enumerate() {
        surface_mut(e, s).unwrap().ptr = ptrs[(i + 1) % ptrs.len()];
    }
    show_if_primary(e, a[0])?;
    Ok(DD_OK)
}

/// A RECT, or the whole surface when the pointer is null; clipped to the surface.
fn rect_or_all(e: &Emu, p: u32, w: u32, h: u32) -> Res<(i32, i32, i32, i32)> {
    if p == 0 {
        return Ok((0, 0, w as i32, h as i32));
    }
    let r = |o: u32| -> Res<i32> { Ok(e.mem.read(p + o, 4)? as i32) };
    Ok((r(0)?, r(4)?, r(8)?, r(12)?))
}

fn blt(e: &mut Emu, a: &Args) -> Res<u32> {
    let (dst, src, flags, fx) = (a[0], a[2], a[4], a[5]);
    let Some(d) = surface(e, dst) else { return Ok(DDERR_INVALIDPARAMS) };
    let (dw, dh) = (d.w, d.h);
    let dr = rect_or_all(e, a[1], dw, dh)?;
    if flags & DDBLT_COLORFILL != 0 {
        let color = e.mem.read(fx + 80, 4)?;
        fill(e, dst, dr, color)?;
    } else if let Some(s) = surface(e, src) {
        let (sw, sh, key) = (s.w, s.h, s.color_key);
        let sr = rect_or_all(e, a[3], sw, sh)?;
        let key = if flags & DDBLT_KEYSRC != 0 { key } else { None };
        copy(e, src, sr, dst, dr, key)?;
    } else {
        return Ok(DDERR_INVALIDPARAMS);
    }
    show_if_primary(e, dst)?;
    Ok(DD_OK)
}

fn blt_fast(e: &mut Emu, a: &Args) -> Res<u32> {
    let (dst, x, y, src, flags) = (a[0], a[1] as i32, a[2] as i32, a[3], a[5]);
    let Some(s) = surface(e, src) else { return Ok(DDERR_INVALIDPARAMS) };
    let (sw, sh, key) = (s.w, s.h, s.color_key);
    let sr = rect_or_all(e, a[4], sw, sh)?;
    let dr = (x, y, x + sr.2 - sr.0, y + sr.3 - sr.1);
    let key = if flags & DDBLTFAST_SRCCOLORKEY != 0 { key } else { None };
    copy(e, src, sr, dst, dr, key)?;
    show_if_primary(e, dst)?;
    Ok(DD_OK)
}

fn fill(e: &mut Emu, dst: u32, r: (i32, i32, i32, i32), color: u32) -> Res<()> {
    let d = surface(e, dst).unwrap();
    let (w, h, pitch, ptr, bytes) = (d.w as i32, d.h as i32, d.pitch, d.ptr, d.format.bpp.div_ceil(8));
    let (x0, y0, x1, y1) = (r.0.max(0), r.1.max(0), r.2.min(w), r.3.min(h));
    if x0 >= x1 || y0 >= y1 {
        return Ok(());
    }
    let px = &color.to_le_bytes()[..bytes as usize];
    let row: Vec<u8> = px.iter().copied().cycle().take(((x1 - x0) as u32 * bytes) as usize).collect();
    for y in y0..y1 {
        e.mem.write_bytes(ptr + y as u32 * pitch + x0 as u32 * bytes, &row)?;
    }
    Ok(())
}

/// Copy (stretching by nearest neighbour if the sizes differ) between surfaces of the same
/// format, skipping source pixels equal to `key`.
fn copy(e: &mut Emu, src: u32, sr: (i32, i32, i32, i32), dst: u32, dr: (i32, i32, i32, i32), key: Option<u32>) -> Res<()> {
    let s = surface(e, src).unwrap();
    let (sp, spitch, bytes) = (s.ptr, s.pitch, s.format.bpp.div_ceil(8) as usize);
    let (sw, sh) = (s.w as i32, s.h as i32);
    let d = surface(e, dst).unwrap();
    let (dp, dpitch) = (d.ptr, d.pitch);
    let (dw, dh) = (d.w as i32, d.h as i32);
    let (rw, rh) = (dr.2 - dr.0, dr.3 - dr.1);
    let (srw, srh) = (sr.2 - sr.0, sr.3 - sr.1);
    if rw <= 0 || rh <= 0 || srw <= 0 || srh <= 0 {
        return Ok(());
    }
    let stretch = rw != srw || rh != srh;
    // Rows are read whole; a same-surface copy reads everything first.
    let src_rows: Vec<Vec<u8>> = (sr.1..sr.3)
        .map(|y| {
            if y < 0 || y >= sh {
                return Ok(Vec::new());
            }
            e.mem.read_bytes(sp + y as u32 * spitch, (sw as usize * bytes) as u32)
        })
        .collect::<Result<_, _>>()?;
    let key_bytes = key.map(|k| k.to_le_bytes());
    for y in dr.1.max(0)..dr.3.min(dh) {
        let sy = ((y - dr.1) as i64 * srh as i64 / rh as i64) as usize;
        let row = &src_rows[sy];
        if row.is_empty() {
            continue;
        }
        let (x0, x1) = (dr.0.max(0), dr.2.min(dw));
        if x0 >= x1 {
            continue;
        }
        let at = dp + y as u32 * dpitch + x0 as u32 * bytes as u32;
        let src_x = |x: i32| -> i32 { sr.0 + if stretch { ((x - dr.0) as i64 * srw as i64 / rw as i64) as i32 } else { x - dr.0 } };
        match key_bytes {
            None if !stretch && src_x(x0) >= 0 && src_x(x1 - 1) < sw => {
                let from = src_x(x0) as usize * bytes;
                e.mem.write_bytes(at, &row[from..from + (x1 - x0) as usize * bytes])?;
            }
            _ => {
                let mut out = e.mem.read_bytes(at, (x1 - x0) as u32 * bytes as u32)?;
                for x in x0..x1 {
                    let sx = src_x(x);
                    if sx < 0 || sx >= sw {
                        continue;
                    }
                    let p = &row[sx as usize * bytes..][..bytes];
                    if key_bytes.is_some_and(|k| &k[..bytes] == p) {
                        continue;
                    }
                    out[(x - x0) as usize * bytes..][..bytes].copy_from_slice(p);
                }
                e.mem.write_bytes(at, &out)?;
            }
        }
    }
    Ok(())
}

// ---- palettes and clippers -------------------------------------------------------------------

fn palette_method(method: &str) -> Handler {
    match method {
        "GetCaps" => |e, a| {
            let flags = match e.win.ddraw.objects.get(&a[0]) {
                Some(Obj::Palette { flags, .. }) => *flags,
                _ => return Ok(DDERR_INVALIDPARAMS),
            };
            e.mem.write(a[1], 4, flags)?;
            Ok(DD_OK)
        },
        "GetEntries" => |e, a| {
            let Some(Obj::Palette { entries, .. }) = e.win.ddraw.objects.get(&a[0]) else { return Ok(DDERR_INVALIDPARAMS) };
            let picked: Vec<u32> = entries.iter().skip(a[2] as usize).take(a[3] as usize).copied().collect();
            for (i, c) in picked.into_iter().enumerate() {
                e.mem.write(a[4] + 4 * i as u32, 4, palette_entry(c))?;
            }
            Ok(DD_OK)
        },
        "SetEntries" => |e, a| {
            let (start, n) = (a[2] as usize, (a[3] as usize).min(256usize.saturating_sub(a[2] as usize)));
            let mut new = Vec::with_capacity(n);
            for i in 0..n {
                new.push(palette_entry(e.mem.read(a[4] + 4 * i as u32, 4)?));
            }
            let Some(Obj::Palette { entries, .. }) = e.win.ddraw.objects.get_mut(&a[0]) else { return Ok(DDERR_INVALIDPARAMS) };
            entries[start..start + n].copy_from_slice(&new);
            // The screen shows the new colours at once.
            let primary = e.win.ddraw.primary;
            if surface(e, primary).is_some_and(|s| s.palette == a[0]) {
                show(e, primary)?;
            }
            Ok(DD_OK)
        },
        _ => ok,
    }
}

fn clipper_method(method: &str) -> Handler {
    match method {
        "SetHWnd" => |e, a| {
            if let Some(Obj::Clipper { hwnd, .. }) = e.win.ddraw.objects.get_mut(&a[0]) {
                *hwnd = a[2];
            }
            Ok(DD_OK)
        },
        "GetHWnd" => |e, a| {
            let h = match e.win.ddraw.objects.get(&a[0]) {
                Some(Obj::Clipper { hwnd, .. }) => *hwnd,
                _ => 0,
            };
            e.mem.write(a[1], 4, h)?;
            Ok(DD_OK)
        },
        _ => ok,
    }
}

// ---- the screen --------------------------------------------------------------------------------

fn show_if_primary(e: &mut Emu, s: u32) -> Res<()> {
    if s != 0 && s == e.win.ddraw.primary {
        show(e, s)?;
    }
    Ok(())
}

/// Copy the primary surface into the game's window, as RGB, scaled to the window's size.
fn show(e: &mut Emu, s: u32) -> Res<()> {
    let Some(sf) = surface(e, s) else { return Ok(()) };
    let (w, h, pitch, ptr, f, pal, clipper) = (sf.w, sf.h, sf.pitch, sf.ptr, sf.format, sf.palette, sf.clipper);
    let palette: [u32; 256] = match e.win.ddraw.objects.get(&pal) {
        Some(Obj::Palette { entries, .. }) => **entries,
        _ => std::array::from_fn(|i| (i as u32) * 0x010101),
    };
    let clip_hwnd = match e.win.ddraw.objects.get(&clipper) {
        Some(Obj::Clipper { hwnd, .. }) => *hwnd,
        _ => 0,
    };
    let hwnd = [clip_hwnd, e.win.ddraw.hwnd]
        .into_iter()
        .find(|&h| h != 0)
        .or_else(|| e.wm.windows.iter().find(|w| w.parent == 0 && w.visible).map(|w| w.hwnd));
    let Some(hwnd) = hwnd else { return Ok(()) };
    let bytes = e.mem.read_bytes(ptr, pitch * h)?;
    let mut rgb = vec![0u32; (w * h) as usize];
    for y in 0..h as usize {
        let row = &bytes[y * pitch as usize..];
        let out = &mut rgb[y * w as usize..][..w as usize];
        match f.bpp {
            8 => out.iter_mut().zip(row).for_each(|(o, &i)| *o = palette[i as usize]),
            16 => {
                for (x, o) in out.iter_mut().enumerate() {
                    let v = u16::from_le_bytes([row[2 * x], row[2 * x + 1]]) as u32;
                    *o = expand(v, f.masks);
                }
            }
            24 => {
                for (x, o) in out.iter_mut().enumerate() {
                    *o = u32::from_le_bytes([row[3 * x], row[3 * x + 1], row[3 * x + 2], 0]);
                }
            }
            _ => {
                for (x, o) in out.iter_mut().enumerate() {
                    *o = u32::from_le_bytes([row[4 * x], row[4 * x + 1], row[4 * x + 2], 0]);
                }
            }
        }
    }
    if let Some(win) = e.wm.window_mut(hwnd) {
        let (sw, sh) = (win.surface.width as usize, win.surface.height as usize);
        let (w, h) = (w as usize, h as usize);
        if sw > 0 && sh > 0 {
            for (y, row) in win.surface.pixels.chunks_exact_mut(sw).take(sh).enumerate() {
                let src = &rgb[(y * h / sh) * w..][..w];
                for (x, o) in row.iter_mut().enumerate() {
                    *o = src[x * w / sw];
                }
            }
        }
        win.needs_paint = false;
    }
    e.win.ddraw.frames += 1;
    Ok(())
}

/// A 16-bit pixel as 0x00RRGGBB, given its channel masks.
fn expand(v: u32, masks: [u32; 3]) -> u32 {
    let ch = |m: u32| -> u32 {
        if m == 0 {
            return 0;
        }
        let c = (v & m) >> m.trailing_zeros();
        let bits = m.count_ones();
        (c << (8 - bits)) | (c >> (2 * bits).saturating_sub(8).min(bits))
    };
    ch(masks[0]) << 16 | ch(masks[1]) << 8 | ch(masks[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb565_expands_to_full_range() {
        let m = [0xF800, 0x07E0, 0x001F];
        assert_eq!(expand(0xFFFF, m), 0xFFFFFF);
        assert_eq!(expand(0, m), 0);
        assert_eq!(expand(0xF800, m), 0xFF0000);
    }

    #[test]
    fn method_tables_match_the_headers() {
        assert_eq!(DD_METHODS.len(), 23);
        assert_eq!(DD2_METHODS.len(), 24);
        assert_eq!(SURFACE_METHODS.len(), 40);
        assert_eq!(SURFACE_METHODS.iter().position(|m| m.0 == "Lock"), Some(25));
        assert_eq!(DD_METHODS.iter().position(|m| m.0 == "EnumDisplayModes"), Some(8));
    }

    /// Fill, keyed blit and lock through the COM entry points, as a game would use them.
    #[test]
    fn blits_and_palettes_reach_the_window() {
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x10000);
        let call = |e: &mut Emu, name: &str, args: &[u32]| -> u32 {
            let (_, h) = lookup("ddraw", name).unwrap();
            crate::hooks::call(e, h, args)
        };
        assert_eq!(call(&mut e, "DirectDrawCreate", &[0, 0x10000, 0]), DD_OK);
        let dd = e.mem.read(0x10000, 4).unwrap();
        call(&mut e, "IDirectDraw::SetDisplayMode", &[dd, 8, 4, 8]);
        // primary: 8x4, 8 bits
        let desc = 0x10100;
        e.mem.write(desc, 4, 108).unwrap();
        e.mem.write(desc + SD_FLAGS, 4, DDSD_CAPS).unwrap();
        e.mem.write(desc + SD_CAPS, 4, DDSCAPS_PRIMARYSURFACE).unwrap();
        assert_eq!(call(&mut e, "IDirectDraw::CreateSurface", &[dd, desc, 0x10004, 0]), DD_OK);
        let primary = e.mem.read(0x10004, 4).unwrap();
        // offscreen 4x4, source key 0
        e.mem.write(desc + SD_FLAGS, 4, DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT | DDSD_CKSRCBLT).unwrap();
        e.mem.write(desc + SD_CAPS, 4, DDSCAPS_OFFSCREENPLAIN).unwrap();
        e.mem.write(desc + SD_WIDTH, 4, 4).unwrap();
        e.mem.write(desc + SD_HEIGHT, 4, 4).unwrap();
        e.mem.write(desc + SD_CKSRCBLT, 4, 0).unwrap();
        call(&mut e, "IDirectDraw::CreateSurface", &[dd, desc, 0x10008, 0]);
        let sprite = e.mem.read(0x10008, 4).unwrap();
        // fill the primary with colour 1, write a sprite pixel 2 at (1,1) and blit it keyed
        let fx = 0x10200;
        e.mem.write(fx + 80, 4, 1).unwrap();
        call(&mut e, "IDirectDrawSurface::Blt", &[primary, 0, 0, 0, DDBLT_COLORFILL, fx]);
        e.mem.write(desc, 4, 108).unwrap();
        call(&mut e, "IDirectDrawSurface::Lock", &[sprite, 0, desc, 0, 0]);
        let (bits, pitch) = (e.mem.read(desc + SD_SURFACE, 4).unwrap(), e.mem.read(desc + SD_PITCH, 4).unwrap());
        e.mem.write(bits + pitch + 1, 1, 2).unwrap();
        call(&mut e, "IDirectDrawSurface::Unlock", &[sprite, 0]);
        call(&mut e, "IDirectDrawSurface::BltFast", &[primary, 4, 0, sprite, 0, DDBLTFAST_SRCCOLORKEY]);
        let p = surface(&e, primary).unwrap();
        let rows: Vec<Vec<u8>> = (0..2).map(|y| e.mem.read_bytes(p.ptr + y * p.pitch, 8).unwrap()).collect();
        assert_eq!(rows[0], [1; 8], "the key colour left the fill showing");
        assert_eq!(rows[1], [1, 1, 1, 1, 1, 2, 1, 1], "the sprite pixel landed at (4+1, 1)");
        // a palette maps the indices to colours
        let entries = 0x10300;
        e.mem.write(entries + 4, 4, 0x0000_00FF).unwrap(); // index 1: red
        call(&mut e, "IDirectDraw::CreatePalette", &[dd, 0x44, entries, 0x1000C, 0]);
        let pal = e.mem.read(0x1000C, 4).unwrap();
        assert_eq!(call(&mut e, "IDirectDrawSurface::SetPalette", &[primary, pal]), DD_OK);
        e.mem.write(0x10400, 4, 0).unwrap();
        call(&mut e, "IDirectDrawPalette::GetEntries", &[pal, 0, 1, 1, 0x10400]);
        assert_eq!(e.mem.read(0x10400, 4).unwrap(), 0xFF, "entries come back in PALETTEENTRY order");
    }
}
