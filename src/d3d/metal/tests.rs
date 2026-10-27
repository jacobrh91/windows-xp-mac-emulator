//! Tests of `metal`.

use super::*;
use crate::d3d::raster::*;

fn vtx(x: f32, y: f32, z: f32, rhw: f32, color: u32) -> Vertex {
    Vertex {
        x,
        y,
        z,
        rhw,
        color: unpack(color),
        spec: [0.0; 4],
        uv: [[0.0; 2]; 2],
    }
}

fn stages() -> [StageState; 2] {
    [StageState::default_for(0), StageState::default_for(1)]
}

/// Render the same triangle list with the CPU reference and with Metal.
fn both(
    w: u32,
    h: u32,
    clear: u32,
    st: &RenderState,
    stg: &[StageState; 2],
    tex: &[Option<(u32, TextureRef)>; 2],
    mem: &Memory,
    tris: &[Vertex],
) -> Option<(Vec<u32>, Vec<u32>)> {
    let mut gpu = Gpu::new(w, h)?;
    let mut cpu = Target::new(w, h);
    cpu.clear(Some(clear), Some(1.0));
    gpu.clear(Some(clear), Some(1.0));
    let ctex = [
        tex[0].as_ref().map(|t| t.1.clone()),
        tex[1].as_ref().map(|t| t.1.clone()),
    ];
    for t in tris.chunks(3) {
        draw_triangle(&mut cpu, mem, st, stg, &ctex, [&t[0], &t[1], &t[2]]);
    }
    gpu.draw(mem, st, stg, tex, tris);
    let mut out = vec![0u32; (w * h) as usize];
    gpu.read_color(&mut out);
    Some((cpu.color, out))
}

/// Pixels may differ by rounding (and on shared edges); most must agree closely.
fn assert_close(cpu: &[u32], gpu: &[u32], what: &str) {
    let mut bad = 0;
    for (a, b) in cpu.iter().zip(gpu) {
        let d = (0..3)
            .map(|k| ((a >> (8 * k)) & 0xFF) as i32 - ((b >> (8 * k)) & 0xFF) as i32)
            .map(|x| x.abs())
            .max()
            .unwrap();
        if d > 4 {
            bad += 1;
        }
    }
    assert!(
        bad * 50 <= cpu.len(),
        "{what}: {bad} of {} pixels differ (cpu[0..4] {:x?}, gpu[0..4] {:x?})",
        cpu.len(),
        &cpu[..4.min(cpu.len())],
        &gpu[..4.min(gpu.len())]
    );
}

fn no_tex() -> [Option<(u32, TextureRef)>; 2] {
    [None, None]
}

#[test]
fn gouraud_triangle_matches_the_reference() {
    let mem = Memory::new();
    let st = RenderState {
        cull: 1,
        zenable: false,
        ..RenderState::default()
    };
    let tri = [
        vtx(2.0, 2.0, 0.5, 1.0, 0xFFFF0000),
        vtx(30.0, 4.0, 0.5, 1.0, 0xFF00FF00),
        vtx(4.0, 30.0, 0.5, 1.0, 0xFF0000FF),
    ];
    let Some((c, g)) = both(32, 32, 0xFF202020, &st, &stages(), &no_tex(), &mem, &tri) else {
        return;
    };
    assert_close(&c, &g, "gouraud");
    assert_eq!(g[0], 0xFF202020, "outside the triangle: clear colour");
}

#[test]
fn culling_follows_the_same_winding_as_the_reference() {
    let mem = Memory::new();
    let cw = [
        vtx(1.0, 1.0, 0.5, 1.0, 0xFFFFFFFF),
        vtx(14.0, 1.0, 0.5, 1.0, 0xFFFFFFFF),
        vtx(1.0, 14.0, 0.5, 1.0, 0xFFFFFFFF),
    ];
    let ccw = [cw[0], cw[2], cw[1]];
    for cull in [1, 2, 3] {
        for (name, tri) in [("cw", cw), ("ccw", ccw)] {
            let st = RenderState {
                cull,
                zenable: false,
                ..RenderState::default()
            };
            let Some((c, g)) = both(16, 16, 0xFF000000, &st, &stages(), &no_tex(), &mem, &tri)
            else {
                return;
            };
            assert_close(&c, &g, &format!("cull {cull} {name}"));
        }
    }
}

#[test]
fn depth_test_matches_the_reference() {
    let mem = Memory::new();
    let st = RenderState {
        cull: 1,
        ..RenderState::default()
    };
    let tri = |z: f32, c: u32| {
        [
            vtx(0.0, 0.0, z, 1.0, c),
            vtx(32.0, 0.0, z, 1.0, c),
            vtx(0.0, 32.0, z, 1.0, c),
        ]
    };
    let mut all = tri(0.2, 0xFFFF0000).to_vec();
    all.extend(tri(0.8, 0xFF0000FF));
    let Some((c, g)) = both(32, 32, 0xFF000000, &st, &stages(), &no_tex(), &mem, &all) else {
        return;
    };
    assert_close(&c, &g, "depth");
    assert_eq!(
        g[5 * 32 + 5],
        0xFFFF0000,
        "nearer triangle wins even though it was drawn first"
    );
}

#[test]
fn blending_and_alpha_test_match_the_reference() {
    let mem = Memory::new();
    let tri = |c: u32| {
        [
            vtx(0.0, 0.0, 0.5, 1.0, c),
            vtx(32.0, 0.0, 0.5, 1.0, c),
            vtx(0.0, 32.0, 0.5, 1.0, c),
            vtx(32.0, 0.0, 0.5, 1.0, c),
            vtx(32.0, 32.0, 0.5, 1.0, c),
            vtx(0.0, 32.0, 0.5, 1.0, c),
        ]
    };
    let blend = RenderState {
        cull: 1,
        zenable: false,
        alpha_blend: true,
        src_blend: 5,
        dst_blend: 6,
        ..RenderState::default()
    };
    let Some((c, g)) = both(
        32,
        32,
        0xFF0000FF,
        &blend,
        &stages(),
        &no_tex(),
        &mem,
        &tri(0x80FF0000),
    ) else {
        return;
    };
    assert_close(&c, &g, "blend");
    let p = unpack(g[40]);
    assert!(
        (p[0] - 0.5).abs() < 0.03 && (p[2] - 0.5).abs() < 0.03,
        "half red over blue: {p:?}"
    );

    let test = RenderState {
        cull: 1,
        zenable: false,
        alpha_test: true,
        alpha_func: 7,
        alpha_ref: 0x80,
        ..RenderState::default()
    };
    let (c, g) = both(
        32,
        32,
        0xFF000000,
        &test,
        &stages(),
        &no_tex(),
        &mem,
        &tri(0x40FFFFFF),
    )
    .unwrap();
    assert_close(&c, &g, "alpha test (fails)");
    assert_eq!(g[40], 0xFF000000, "alpha 0x40 < ref 0x80: discarded");
    let (c, g) = both(
        32,
        32,
        0xFF000000,
        &test,
        &stages(),
        &no_tex(),
        &mem,
        &tri(0xC0FFFFFF),
    )
    .unwrap();
    assert_close(&c, &g, "alpha test (passes)");
}

#[test]
fn perspective_correct_interpolation_matches_the_reference() {
    let mem = Memory::new();
    let st = RenderState {
        cull: 1,
        zenable: false,
        ..RenderState::default()
    };
    let tri = [
        vtx(0.0, 0.0, 0.5, 1.0, 0xFFFF0000),
        vtx(32.0, 0.0, 0.5, 0.2, 0xFF00FF00),
        vtx(0.0, 32.0, 0.5, 1.0, 0xFF0000FF),
    ];
    let Some((c, g)) = both(32, 32, 0xFF000000, &st, &stages(), &no_tex(), &mem, &tri) else {
        return;
    };
    assert_close(&c, &g, "perspective");
}

fn texture(mem: &mut Memory, w: u32, h: u32, px: &[u32]) -> TextureRef {
    mem.map(0x10000, 0x10000);
    let bytes: Vec<u8> = px.iter().flat_map(|c| c.to_le_bytes()).collect();
    mem.write_bytes(0x10000, &bytes).unwrap();
    TextureRef {
        format: FMT_A8R8G8B8,
        levels: vec![Level {
            ptr: 0x10000,
            w,
            h,
            pitch: w * 4,
        }],
    }
}

#[test]
fn textured_quad_matches_the_reference() {
    let mut mem = Memory::new();
    let t = texture(
        &mut mem,
        2,
        2,
        &[0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF],
    );
    let mk = |x: f32, y: f32, u: f32, v: f32| {
        let mut vt = vtx(x, y, 0.5, 1.0, 0xFFFFFFFF);
        vt.uv[0] = [u, v];
        vt
    };
    let quad = [
        mk(0.0, 0.0, 0.0, 0.0),
        mk(16.0, 0.0, 1.0, 0.0),
        mk(0.0, 16.0, 0.0, 1.0),
        mk(16.0, 0.0, 1.0, 0.0),
        mk(16.0, 16.0, 1.0, 1.0),
        mk(0.0, 16.0, 0.0, 1.0),
    ];
    let st = RenderState {
        cull: 1,
        zenable: false,
        ..RenderState::default()
    };
    let tex = [Some((7, t)), None];
    // point sampling
    let Some((c, g)) = both(16, 16, 0xFF000000, &st, &stages(), &tex, &mem, &quad) else {
        return;
    };
    assert_close(&c, &g, "point sampled texture");
    assert_eq!(
        (g[0], g[15], g[15 * 16], g[255]),
        (0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF)
    );
    // bilinear
    let mut linear = stages();
    linear[0].mag = 2;
    linear[0].min = 2;
    let (c, g) = both(16, 16, 0xFF000000, &st, &linear, &tex, &mem, &quad).unwrap();
    assert_close(&c, &g, "bilinear texture");
}

#[test]
fn two_stage_modulate2x_matches_the_reference() {
    let mut mem = Memory::new();
    let t = texture(&mut mem, 1, 1, &[0xFF808080]);
    let mk = |x: f32, y: f32| {
        let mut vt = vtx(x, y, 0.5, 1.0, 0xFF808080);
        vt.uv = [[0.5, 0.5]; 2];
        vt
    };
    let quad = [mk(0.0, 0.0), mk(16.0, 0.0), mk(0.0, 16.0)];
    let mut stg = stages();
    stg[0].color_op = 4; // MODULATE with diffuse
    stg[1].color_op = 5; // MODULATE2X with current
    stg[1].color_arg1 = 2;
    stg[1].color_arg2 = 1;
    stg[1].alpha_op = 2;
    let st = RenderState {
        cull: 1,
        zenable: false,
        ..RenderState::default()
    };
    let tex = [Some((1, t.clone())), Some((2, t))];
    let Some((c, g)) = both(16, 16, 0xFF000000, &st, &stg, &tex, &mem, &quad) else {
        return;
    };
    assert_close(&c, &g, "two stages");
}

#[test]
fn clearing_between_draws_orders_correctly() {
    let Some(mut gpu) = Gpu::new(8, 8) else {
        return;
    };
    let mem = Memory::new();
    let st = RenderState {
        cull: 1,
        zenable: false,
        ..RenderState::default()
    };
    let tri = [
        vtx(0.0, 0.0, 0.5, 1.0, 0xFFFFFFFF),
        vtx(16.0, 0.0, 0.5, 1.0, 0xFFFFFFFF),
        vtx(0.0, 16.0, 0.5, 1.0, 0xFFFFFFFF),
    ];
    gpu.clear(Some(0xFF000000), Some(1.0));
    gpu.draw(&mem, &st, &stages(), &no_tex(), &tri);
    gpu.clear(Some(0xFF00FF00), None); // wipes the white triangle
    let mut out = vec![0u32; 64];
    gpu.read_color(&mut out);
    assert!(
        out.iter().all(|&p| p == 0xFF00FF00),
        "the later clear covers the earlier draw"
    );
    gpu.write_color(&vec![0xFF123456; 64]);
    gpu.read_color(&mut out);
    assert!(
        out.iter().all(|&p| p == 0xFF123456),
        "write_color/read_color round-trip"
    );
}
