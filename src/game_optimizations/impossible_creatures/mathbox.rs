//! `MathBox.dll`'s (SSE build) vertex loops, run for every animated creature each frame.

use crate::cpu::{Emu, Res};
use crate::hooks::Hook;
use crate::win32::{Args, Handler};

pub(super) const HOOKS: &[Hook] = &[
    // The skinned-vertex transform loop.
    Hook {
        module: "mathbox",
        rva: 0x1c59,
        expect: &[
            0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0, 0x83, 0xEC, 0x1C, 0x56, 0x8B, 0x0D,
        ],
        name: "MathBox_skin_vertices",
        points_to: Some((0x1c65, MB_SRC)),
        on_patch: Some(|e, base| e.win.games.impossible_creatures.mathbox.base = base),
    },
    // The same loop for vertices with a position only.
    Hook {
        module: "mathbox",
        rva: 0x1fa0,
        expect: &[0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0, 0x83, 0xEC, 0x18, 0xA1],
        name: "MathBox_rigid_vertices",
        points_to: Some((0x1faa, MB_SRC)),
        on_patch: Some(|e, base| e.win.games.impossible_creatures.mathbox.base = base),
    },
];

#[derive(Default)]
pub struct State {
    /// Where MathBox.dll is loaded (its globals are addressed from here).
    base: u32,
}

pub(super) fn lookup(name: &str) -> Option<(usize, Handler)> {
    Some(match name {
        "MathBox_skin_vertices" => (1, skin_vertices),
        "MathBox_rigid_vertices" => (1, rigid_vertices),
        _ => return None,
    })
}

// ---- skinned vertices -----------------------------------------------------------------
//
// The loop's state lives in MathBox globals (rvas): source and destination pointers, their
// strides, the offsets of a two-dword field copied through unchanged (texture coordinates),
// and the vertex count. Each source vertex holds a position (x, y, z at 0), a bone index byte
// (at 12) and a normal (at 32); the bone selects a 4x4 matrix (rows of four floats) from a
// palette. Positions get the full transform, normals the rotation only, computed with the
// same single-precision operations in the same order as the original SSE code.

const MB_SRC: u32 = 0x70b0;
const MB_SRC_STRIDE: u32 = 0x70b4;
const MB_SRC_COPY: u32 = 0x70b8;
const MB_DST: u32 = 0x70bc;
const MB_DST_STRIDE: u32 = 0x70c0;
const MB_DST_COPY: u32 = 0x70c4;
const MB_COUNT: u32 = 0x70cc;
const MB_PALETTE: u32 = 0x50b0;

fn skin_vertices(e: &mut Emu, _: &Args) -> Res<u32> {
    let b = e.win.games.impossible_creatures.mathbox.base;
    let rd = |e: &Emu, a: u32| e.mem.read(a, 4);
    let rf = |e: &Emu, a: u32| -> Res<f32> { Ok(f32::from_bits(e.mem.read(a, 4)?)) };
    let row = |e: &Emu, m: u32, r: u32| -> Res<[f32; 4]> {
        Ok([
            rf(e, m + 16 * r)?,
            rf(e, m + 16 * r + 4)?,
            rf(e, m + 16 * r + 8)?,
            rf(e, m + 16 * r + 12)?,
        ])
    };
    loop {
        let src = rd(e, b + MB_SRC)?;
        let m = b + MB_PALETTE + 64 * e.mem.read_u8(src + 12)? as u32;
        let (m0, m1, m2, m3) = (row(e, m, 0)?, row(e, m, 1)?, row(e, m, 2)?, row(e, m, 3)?);
        // x*M0 + (y*M1 + z*M2), plus M3 for positions
        let xform =
            |x: f32, y: f32, z: f32| [0, 1, 2, 3].map(|i| x * m0[i] + (y * m1[i] + z * m2[i]));
        let p = xform(rf(e, src)?, rf(e, src + 4)?, rf(e, src + 8)?);
        let dst = rd(e, b + MB_DST)?;
        for i in 0..4 {
            e.mem
                .write(dst + 4 * i as u32, 4, (m3[i] + p[i]).to_bits())?;
        }
        // The normal's 16-byte store starts at dst + 12, over the position's fourth lane.
        let n = xform(rf(e, src + 32)?, rf(e, src + 36)?, rf(e, src + 40)?);
        for i in 0..4 {
            e.mem.write(dst + 12 + 4 * i as u32, 4, n[i].to_bits())?;
        }
        let (from, to) = (src + rd(e, b + MB_SRC_COPY)?, dst + rd(e, b + MB_DST_COPY)?);
        let (c0, c1) = (rd(e, from)?, rd(e, from + 4)?);
        e.mem.write(to, 4, c0)?;
        e.mem.write(to + 4, 4, c1)?;
        e.mem
            .write(b + MB_SRC, 4, src.wrapping_add(rd(e, b + MB_SRC_STRIDE)?))?;
        e.mem
            .write(b + MB_DST, 4, dst.wrapping_add(rd(e, b + MB_DST_STRIDE)?))?;
        let count = rd(e, b + MB_COUNT)?.wrapping_sub(1);
        e.mem.write(b + MB_COUNT, 4, count)?;
        if count == 0 {
            return Ok(0);
        }
    }
}

/// The rigid loop: as `skin_vertices`, but a position only (no normal), and five dwords
/// copied through unchanged instead of two.
fn rigid_vertices(e: &mut Emu, _: &Args) -> Res<u32> {
    let b = e.win.games.impossible_creatures.mathbox.base;
    let rd = |e: &Emu, a: u32| e.mem.read(a, 4);
    let rf = |e: &Emu, a: u32| -> Res<f32> { Ok(f32::from_bits(e.mem.read(a, 4)?)) };
    loop {
        let src = rd(e, b + MB_SRC)?;
        let m = b + MB_PALETTE + 64 * e.mem.read_u8(src + 12)? as u32;
        let (x, y, z) = (rf(e, src)?, rf(e, src + 4)?, rf(e, src + 8)?);
        let dst = rd(e, b + MB_DST)?;
        for i in 0..4 {
            let c = |r: u32| rf(e, m + 16 * r + 4 * i);
            // M3 + (x*M0 + (y*M1 + z*M2))
            let v = c(3)? + (x * c(0)? + (y * c(1)? + z * c(2)?));
            e.mem.write(dst + 4 * i, 4, v.to_bits())?;
        }
        let (from, to) = (src + rd(e, b + MB_SRC_COPY)?, dst + rd(e, b + MB_DST_COPY)?);
        for k in 0..5 {
            let v = rd(e, from + 4 * k)?;
            e.mem.write(to + 4 * k, 4, v)?;
        }
        e.mem
            .write(b + MB_SRC, 4, src.wrapping_add(rd(e, b + MB_SRC_STRIDE)?))?;
        e.mem
            .write(b + MB_DST, 4, dst.wrapping_add(rd(e, b + MB_DST_STRIDE)?))?;
        let count = rd(e, b + MB_COUNT)?.wrapping_sub(1);
        e.mem.write(b + MB_COUNT, 4, count)?;
        if count == 0 {
            return Ok(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native skinning loop against MathBox's own code, on the same data, bit for bit.
    #[test]
    fn skinning_matches_mathbox_exactly() {
        matches_mathbox(0x1c59, "MathBox_skin_vertices", 28);
    }

    /// The same for the position-only loop.
    #[test]
    fn rigid_vertices_match_mathbox_exactly() {
        matches_mathbox(0x1fa0, "MathBox_rigid_vertices", 16);
    }

    fn matches_mathbox(rva: u32, name: &str, dst_copy: u32) {
        let path = std::path::Path::new("exe/ic/app/MathBox.dll");
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let mut e = Emu::new();
        e.win.dll_dirs.push(path.parent().unwrap().to_path_buf());
        let base =
            crate::loader::load_dll(&mut e, &bytes, "MathBox.dll", r"C:\Game\MathBox.dll").unwrap();
        assert_eq!(
            e.win.games.impossible_creatures.mathbox.base, base,
            "the hook went in"
        );
        let (src, dst, n, src_stride, dst_stride) =
            (0x0050_0000u32, 0x0060_0000u32, 37u32, 48u32, 40u32);
        e.mem.map(src, 0x10000);
        e.mem.map(dst, 0x10000);
        // Pseudo-random but repeatable data.
        let mut seed = 0x1234_5678u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 8.0 - 4.0
        };
        for i in 0..4 * 16 {
            e.mem
                .write(base + MB_PALETTE + 4 * i, 4, next().to_bits())
                .unwrap();
        }
        for v in 0..n {
            let p = src + v * src_stride;
            for k in [0u32, 4, 8, 32, 36, 40, 16, 20] {
                e.mem.write(p + k, 4, next().to_bits()).unwrap();
            }
            e.mem.write(p + 12, 1, v % 4).unwrap();
        }
        let setup = |e: &mut Emu| {
            for (off, v) in [
                (MB_SRC, src),
                (MB_SRC_STRIDE, src_stride),
                (MB_SRC_COPY, 16),
                (MB_DST, dst),
                (MB_DST_STRIDE, dst_stride),
                (MB_DST_COPY, dst_copy),
                (MB_COUNT, n),
            ] {
                e.mem.write(base + off, 4, v).unwrap();
            }
            e.mem.write_bytes(dst, &[0; 0x2000]).unwrap();
        };
        setup(&mut e);
        e.call_function(base + rva, &[0]).unwrap();
        let native = e.mem.read_bytes(dst, n * dst_stride).unwrap();
        let after = [MB_SRC, MB_DST, MB_COUNT].map(|o| e.mem.read(base + o, 4).unwrap());
        // Put the original code back and run it on the same input.
        e.mem
            .write_bytes(
                base + rva,
                &HOOKS.iter().find(|h| h.name == name).unwrap().expect[..5],
            )
            .unwrap();
        setup(&mut e);
        e.call_function(base + rva, &[0]).unwrap();
        assert_eq!(
            e.mem.read_bytes(dst, n * dst_stride).unwrap(),
            native,
            "identical output, bit for bit"
        );
        assert_eq!(
            [MB_SRC, MB_DST, MB_COUNT].map(|o| e.mem.read(base + o, 4).unwrap()),
            after,
            "and identical loop state afterwards"
        );
    }
}
