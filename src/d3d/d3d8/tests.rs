//! Tests of `d3d8`.

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
    let slot =
        |table: &[(&str, usize)], name: &str| table.iter().position(|(n, _)| *n == name).unwrap();
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
