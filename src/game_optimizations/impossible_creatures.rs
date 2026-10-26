//! Impossible Creatures (Relic, 2003).
//!
//! `Filesystem.dll` carries zlib 1.1.3 and inflates every compressed file in the game's
//! archives through it; under the interpreter that made loading take minutes, so those
//! functions go to `hooks::zlib`. `MathBox.dll`'s vertex loops, run for every animated
//! creature each frame, and the inner loop of the Miles mixer (`mss32.dll`) that resamples
//! every playing sound are replaced below.
//!
//! One fix to the game itself: in campaign mission 5 creatures take the burning fires for
//! enemies and walk into them to fight them (a bug of the game, seen on Windows too when the
//! campaign runs through a mod). `ICMod.dll`'s attack check is wrapped so that a fire is only
//! ever a target for water artillery, from beyond its minimum range: never for melee.

use crate::cpu::{Emu, Res};
use crate::hooks::Hook;
use crate::win32::{Args, Handler};

pub(super) const HOOKS: &[Hook] = &[
    Hook {
        module: "filesystem",
        rva: 0xbc1c,
        expect: &[
            0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0x6A, 0x0F, 0xFF, 0x74, 0x24, 0x10,
        ],
        name: "inflateInit_",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "filesystem",
        rva: 0xbc32,
        expect: &[
            0x53, 0x55, 0x56, 0x8B, 0x74, 0x24, 0x10, 0x33, 0xDB, 0x3B, 0xF3, 0x57,
        ],
        name: "inflate",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "filesystem",
        rva: 0xbb01,
        expect: &[
            0x56, 0x8B, 0x74, 0x24, 0x08, 0x85, 0xF6, 0x74, 0x2C, 0x8B, 0x46, 0x1C,
        ],
        name: "inflateEnd",
        points_to: None,
        on_patch: None,
    },
    // MathBox (SSE build): the skinned-vertex transform loop.
    Hook {
        module: "mathbox",
        rva: 0x1c59,
        expect: &[
            0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0, 0x83, 0xEC, 0x1C, 0x56, 0x8B, 0x0D,
        ],
        name: "MathBox_skin_vertices",
        points_to: Some((0x1c65, MB_SRC)),
        on_patch: Some(|e, base| e.win.games.impossible_creatures.mathbox_base = base),
    },
    // MathBox (SSE build): the same loop for vertices with a position only.
    Hook {
        module: "mathbox",
        rva: 0x1fa0,
        expect: &[0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0, 0x83, 0xEC, 0x18, 0xA1],
        name: "MathBox_rigid_vertices",
        points_to: Some((0x1faa, MB_SRC)),
        on_patch: Some(|e, base| e.win.games.impossible_creatures.mathbox_base = base),
    },
    // ICMod: CanAttack(attacker in eax, target), checked before it runs (`can_attack`).
    Hook {
        module: "icmod",
        rva: CAN_ATTACK,
        expect: &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14, 0x56, 0x8B, 0xF0, 0x56, 0xE8],
        name: "ICMod_can_attack",
        points_to: None,
        on_patch: Some(icmod_patched),
    },
    // ICMod: the attack state's choice of attack record for its target (`choose_attack`).
    Hook {
        module: "icmod",
        rva: CHOOSE_ATTACK,
        expect: &[0xB8, 0xD8, 0x61, 0x1B, 0x40, 0xE8, 0xDE, 0x4B, 0x04, 0x00],
        name: "ICMod_choose_attack",
        points_to: None,
        on_patch: Some(|e, base| {
            let t = trampoline(e, base, CHOOSE_ATTACK, &CHOOSE_ATTACK_PROLOGUE);
            e.win.games.impossible_creatures.choose_attack_original = t;
        }),
    },
    // Miles (mss32.dll 6.x): mixing a 16-bit mono sound into the stereo buffer.
    Hook {
        module: "mss32",
        rva: 0x3c2e3,
        expect: &[
            0xC1, 0xE2, 0x10, 0x0F, 0xBF, 0x06, 0x8B, 0x0D, 0x3C, 0xD2, 0x14, 0x21, 0xA3,
        ],
        name: "Miles_mix_mono16",
        points_to: Some((0x3c2eb, MIX_PREV)),
        on_patch: Some(|e, base| e.win.games.impossible_creatures.mss_base = base),
    },
];

#[derive(Default)]
pub struct State {
    /// Where MathBox.dll is loaded (its globals are addressed from here).
    mathbox_base: u32,
    /// Where mss32.dll is loaded.
    mss_base: u32,
    /// Where ICMod.dll is loaded, and the trampoline that runs the original CanAttack.
    icmod_base: u32,
    can_attack_original: u32,
    /// The trampoline that runs the original attack choice.
    choose_attack_original: u32,
}

pub(super) fn lookup(name: &str) -> Option<(usize, Handler)> {
    Some(match name {
        "MathBox_skin_vertices" => (1, skin_vertices),
        "MathBox_rigid_vertices" => (1, rigid_vertices),
        "Miles_mix_mono16" => (0, mix_mono16),
        "ICMod_can_attack" => (1, can_attack),
        "ICMod_choose_attack" => (0, choose_attack),
        _ => return None,
    })
}

// ---- MathBox: skinned vertices -----------------------------------------------------------
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
    let b = e.win.games.impossible_creatures.mathbox_base;
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
    let b = e.win.games.impossible_creatures.mathbox_base;
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

// ---- Miles: mixing a mono 16-bit sound -------------------------------------------------------
//
// Called with the source sample pointer in esi, the stereo 32-bit mix buffer in edi and the
// position's 16-bit fraction in dx. Each output frame interpolates linearly between the
// previous and current sample, scales it by the left and right volumes and adds it in; the
// 16.16 position advances by the step, reading a new sample on each carry. It returns with
// esi, edi and edx (the fraction) where it stopped, and the state in globals (rvas).

const MIX_STEP: u32 = 0x4d210;
const MIX_VOL_L: u32 = 0x4d220;
const MIX_VOL_R: u32 = 0x4d224;
const MIX_SRC_END: u32 = 0x4d228;
const MIX_DST_END: u32 = 0x4d22c;
const MIX_PREV: u32 = 0x4d23c;

fn mix_mono16(e: &mut Emu, _: &Args) -> Res<u32> {
    use crate::cpu::{EBX, ECX, EDI, EDX, ESI};
    let b = e.win.games.impossible_creatures.mss_base;
    let g = |e: &Emu, o: u32| e.mem.read(b + o, 4);
    let (step, vol_l, vol_r) = (g(e, MIX_STEP)?, g(e, MIX_VOL_L)?, g(e, MIX_VOL_R)?);
    let (src_end, dst_end) = (g(e, MIX_SRC_END)?, g(e, MIX_DST_END)?);
    let (mut esi, mut edi, mut edx) = (e.regs[ESI], e.regs[EDI], e.regs[EDX] << 16);
    let sample = |e: &Emu, a: u32| -> Res<u32> { Ok(e.mem.read(a, 2)? as u16 as i16 as i32 as u32) };
    let mut cur = sample(e, esi)?;
    let mut prev = g(e, MIX_PREV)?;
    let (mut eax, mut ebx);
    loop {
        let frac = edx >> 16;
        let v = ((cur.wrapping_sub(prev).wrapping_mul(frac) as i32) >> 16) as u32;
        let v = v.wrapping_add(prev);
        (eax, ebx) = (v.wrapping_mul(vol_l), v.wrapping_mul(vol_r));
        let (l, r) = (e.mem.read(edi, 4)?, e.mem.read(edi + 4, 4)?);
        e.mem.write(edi, 4, l.wrapping_add(eax))?;
        e.mem.write(edi + 4, 4, r.wrapping_add(ebx))?;
        edi = edi.wrapping_add(8);
        if edi >= dst_end {
            // Out of room: step past this sample only if the position has moved on from it.
            let (next, carry) = edx.overflowing_add(step);
            edx = next;
            if carry {
                esi = esi.wrapping_add(2);
            } else {
                cur = prev;
            }
            break;
        }
        let (next, carry) = edx.overflowing_add(step);
        edx = next;
        if carry {
            esi = esi.wrapping_add(2);
            if esi >= src_end {
                break;
            }
            prev = cur;
            cur = sample(e, esi)?;
        }
    }
    e.mem.write(b + MIX_PREV, 4, cur)?;
    e.regs[ESI] = esi;
    e.regs[EDI] = edi;
    e.regs[EDX] = edx >> 16;
    e.regs[ECX] = prev;
    e.regs[EBX] = ebx;
    Ok(eax)
}

// ---- ICMod: fires are put out with water, never fought in melee ---------------------------
//
// In campaign mission 5 creatures take the fires for enemies and fight them in melee, burning
// to death. Two checks of ICMod.dll are wrapped:
// - CanAttack (stdcall, attacker entity in eax, target entity on the stack, answer in al),
//   which a creature's target search asks of every nearby entity: a fire (an entity whose
//   ICMod controller is a FireController) is only a target for a creature with water
//   artillery, a ranged attack whose `special` attribute is 2 (the Archerfish's, and only
//   its). The attribute loader keeps each ranged attack as a 96-byte record in the combat
//   extension's list at +12..+16, `special` at offset 36.
// - The attack state's choice of attack for its target: for a fire there is no melee attack
//   to choose, whatever the creature has. Without it, one too close for its water artillery
//   (inside the minimum range) falls back to melee, runs into the fire and burns.

const CAN_ATTACK: u32 = 0x72c17;
/// The original's first instructions (push ebp; mov ebp, esp; sub esp, 20), which the patch
/// overwrites, run from the trampoline before it jumps back.
const CAN_ATTACK_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x14];
/// FireController's vtable, and the function returning a controller's combat extension.
const FIRE_CONTROLLER_VTABLE: u32 = 0xdf13c;
const COMBAT_EXT: u32 = 0x16e2c;
const RANGED_ATTACKS: u32 = 12;
const ATTACK_RECORD: u32 = 96;
const ATTACK_SPECIAL: u32 = 36;
const SPECIAL_WATER: u32 = 2;
/// The attack state's choice of attack record (thiscall, `this` in ecx, answer in al: whether
/// one was chosen), its overwritten first instruction (mov eax, imm32), the function giving
/// the state's current target entity, and where the state keeps its attack lists (the melee
/// vector's begin and end are the first two dwords of that block).
const CHOOSE_ATTACK: u32 = 0x7d148;
const CHOOSE_ATTACK_PROLOGUE: [u8; 5] = [0xB8, 0xD8, 0x61, 0x1B, 0x40];
const CURRENT_TARGET: u32 = 0x7c9d3;
const STATE_ATTACK_LISTS: u32 = 88;
/// An entity's ICMod controller.
const ENTITY_CONTROLLER: u32 = 176;

fn icmod_patched(e: &mut Emu, base: u32) {
    let t = trampoline(e, base, CAN_ATTACK, &CAN_ATTACK_PROLOGUE);
    let st = &mut e.win.games.impossible_creatures;
    (st.icmod_base, st.can_attack_original) = (base, t);
}

/// Code that runs a patched function's overwritten first instructions (`prologue`) and jumps
/// back to the rest of it: calling it runs the original. It gets a page of its own, as code
/// on a page also written to as data would throw away the instruction caches on every write.
fn trampoline(e: &mut Emu, base: u32, rva: u32, prologue: &[u8]) -> u32 {
    let at = e.reserve(0x1000);
    e.mem.map(at, 0x1000);
    let n = prologue.len() as u32;
    let back = (base + rva + n).wrapping_sub(at + n + 5);
    let mut code = prologue.to_vec();
    code.push(0xE9); // jmp rel32 back into the original
    code.extend(back.to_le_bytes());
    let _ = e.mem.write_bytes(at, &code);
    at
}

/// Is the entity one of the fires (its ICMod controller a FireController)?
fn is_fire(e: &Emu, base: u32, entity: u32) -> bool {
    let rd = |p: u32| e.mem.read(p, 4);
    entity != 0
        && rd(entity + ENTITY_CONTROLLER)
            .and_then(|c| if c == 0 { Ok(0) } else { rd(c) })
            .is_ok_and(|vt| vt == base + FIRE_CONTROLLER_VTABLE)
}

fn can_attack(e: &mut Emu, a: &Args) -> Res<u32> {
    use crate::cpu::EAX;
    let (attacker, target) = (e.regs[EAX], a[0]);
    let st = &e.win.games.impossible_creatures;
    let (base, original) = (st.icmod_base, st.can_attack_original);
    if is_fire(e, base, target) && !has_water_attack(e, base, attacker)? {
        return Ok(e.regs[EAX] & !0xFF); // al = false
    }
    e.regs[EAX] = attacker;
    e.call_function(original, &[target])
}

fn choose_attack(e: &mut Emu, _: &Args) -> Res<u32> {
    use crate::cpu::ECX;
    let this = e.regs[ECX];
    let st = &e.win.games.impossible_creatures;
    let (base, original) = (st.icmod_base, st.choose_attack_original);
    let target = e.call_function(base + CURRENT_TARGET, &[])?;
    e.regs[ECX] = this;
    if !is_fire(e, base, target) {
        return e.call_function(original, &[]);
    }
    // The melee list is emptied for the length of the choice: with no melee attack to fall
    // back on, a creature too close for its water artillery chooses nothing instead of
    // walking into the flames.
    let lists = e.mem.read(this + STATE_ATTACK_LISTS, 4)?;
    let melee_end = e.mem.read(lists + 4, 4)?;
    let melee_begin = e.mem.read(lists, 4)?;
    e.mem.write(lists + 4, 4, melee_begin)?;
    let r = e.call_function(original, &[]);
    e.mem.write(lists + 4, 4, melee_end)?;
    r
}

/// Does the creature have a ranged attack that spits water?
fn has_water_attack(e: &mut Emu, base: u32, entity: u32) -> Res<bool> {
    if entity == 0 {
        return Ok(false);
    }
    let controller = e.mem.read(entity + ENTITY_CONTROLLER, 4)?;
    let combat = e.call_function(base + COMBAT_EXT, &[controller])?;
    if combat == 0 {
        return Ok(false);
    }
    let (first, end) = (
        e.mem.read(combat + RANGED_ATTACKS, 4)?,
        e.mem.read(combat + RANGED_ATTACKS + 4, 4)?,
    );
    let mut at = first;
    while at < end && end - at < 64 * ATTACK_RECORD {
        if e.mem.read(at + ATTACK_SPECIAL, 4)? == SPECIAL_WATER {
            return Ok(true);
        }
        at += ATTACK_RECORD;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::apply;

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
        assert_eq!(e.win.games.impossible_creatures.mathbox_base, base, "the hook went in");
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
            .write_bytes(base + rva, &HOOKS.iter().find(|h| h.name == name).unwrap().expect[..5])
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

    /// The native mixing loop against Miles' own, on the same sound, including where each
    /// stops: the mix buffer filling up, and the sound running out.
    #[test]
    fn mono16_mixing_matches_miles_exactly() {
        use crate::cpu::{EAX, EBX, ECX, EDI, EDX, ESI};
        let path = std::path::Path::new("exe/ic/app/mss32.dll");
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let mut e = Emu::new();
        e.win.dll_dirs.push(path.parent().unwrap().to_path_buf());
        let base = crate::loader::load_dll(&mut e, &bytes, "mss32.dll", r"C:\Game\mss32.dll").unwrap();
        assert_eq!(e.win.games.impossible_creatures.mss_base, base, "the hook went in");
        if e.regs[crate::cpu::ESP] == 0 {
            crate::loader::init_process(&mut e).unwrap();
        }
        let (src, dst) = (0x0050_0000u32, 0x0060_0000u32);
        e.mem.map(src, 0x10000);
        e.mem.map(dst, 0x10000);
        let mut seed = 0x2468_ace0u32;
        for i in 0..0x800 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            e.mem.write(src + 2 * i, 2, seed >> 16).unwrap();
        }
        // (step, source samples, destination frames, starting fraction)
        for (step, samples, frames, frac) in [
            (0x0000_8000u32, 300u32, 200u32, 0x1234u32), // upsampling, buffer fills first
            (0x0001_8000, 100, 400, 0x0),                // downsampling, sound runs out
            (0x0001_0000, 64, 64, 0xffff),
            (0x0000_5555, 1000, 37, 0x8000),
        ] {
            let run = |e: &mut Emu| {
                for (off, v) in [
                    (MIX_STEP, step),
                    (MIX_VOL_L, 0x1234),
                    (MIX_VOL_R, 0x7f),
                    (MIX_SRC_END, src + 2 * samples),
                    (MIX_DST_END, dst + 8 * frames),
                    (MIX_PREV, 0xffff_fc18),
                ] {
                    e.mem.write(base + off, 4, v).unwrap();
                }
                for i in 0..2 * frames {
                    e.mem.write(dst + 4 * i, 4, i.wrapping_mul(0x0101_0101)).unwrap();
                }
                (e.regs[ESI], e.regs[EDI], e.regs[EDX]) = (src + 2, dst, frac);
                e.call_function(base + 0x3c2e3, &[]).unwrap();
                (
                    e.mem.read_bytes(dst, 8 * frames).unwrap(),
                    e.mem.read(base + MIX_PREV, 4).unwrap(),
                    [ESI, EDI, EDX, ECX, EAX, EBX].map(|r| e.regs[r]),
                )
            };
            let native = run(&mut e);
            let h = HOOKS.iter().find(|h| h.name == "Miles_mix_mono16").unwrap();
            e.mem.write_bytes(base + h.rva, &h.expect[..5]).unwrap();
            assert_eq!(run(&mut e), native, "step {step:#x}: identical mix, state and registers");
            crate::hooks::apply(&mut e, "mss32", base);
        }
    }

    #[test]
    fn only_matching_code_is_patched() {
        let mut e = Emu::new();
        e.mem.map(0x40030000, 0x20000);
        assert_eq!(
            apply(&mut e, "filesystem", 0x40030000),
            0,
            "zeros do not look like zlib"
        );
        let h = &HOOKS[1];
        e.mem.write_bytes(0x40030000 + h.rva, h.expect).unwrap();
        assert_eq!(apply(&mut e, "filesystem", 0x40030000), 1);
        assert_eq!(
            e.mem.read_u8(0x40030000 + h.rva).unwrap(),
            0xE9,
            "now a jump to the stub"
        );
        assert_eq!(apply(&mut e, "otherdll", 0x40030000), 0);
    }
}
