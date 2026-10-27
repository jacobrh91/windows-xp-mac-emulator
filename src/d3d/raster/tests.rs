//! Tests of `raster`.

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
