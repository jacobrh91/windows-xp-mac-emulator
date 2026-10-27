//! The Miles Sound System (`mss32.dll` 6.x): the software mixer's inner loop.

use crate::cpu::{Emu, Res};
use crate::hooks::Hook;
use crate::win32::{Args, Handler};

pub(super) const HOOKS: &[Hook] = &[
    // Mixing a 16-bit mono sound into the stereo buffer.
    Hook {
        module: "mss32",
        rva: 0x3c2e3,
        expect: &[
            0xC1, 0xE2, 0x10, 0x0F, 0xBF, 0x06, 0x8B, 0x0D, 0x3C, 0xD2, 0x14, 0x21, 0xA3,
        ],
        name: "Miles_mix_mono16",
        points_to: Some((0x3c2eb, MIX_PREV)),
        on_patch: Some(|e, base| e.win.games.impossible_creatures.miles.base = base),
    },
];

#[derive(Default)]
pub struct State {
    /// Where mss32.dll is loaded.
    base: u32,
}

pub(super) fn lookup(name: &str) -> Option<(usize, Handler)> {
    Some(match name {
        "Miles_mix_mono16" => (0, mix_mono16),
        _ => return None,
    })
}

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
    let b = e.win.games.impossible_creatures.miles.base;
    let g = |e: &Emu, o: u32| e.mem.read(b + o, 4);
    let (step, vol_l, vol_r) = (g(e, MIX_STEP)?, g(e, MIX_VOL_L)?, g(e, MIX_VOL_R)?);
    let (src_end, dst_end) = (g(e, MIX_SRC_END)?, g(e, MIX_DST_END)?);
    let (mut esi, mut edi, mut edx) = (e.regs[ESI], e.regs[EDI], e.regs[EDX] << 16);
    let sample =
        |e: &Emu, a: u32| -> Res<u32> { Ok(e.mem.read(a, 2)? as u16 as i16 as i32 as u32) };
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let base =
            crate::loader::load_dll(&mut e, &bytes, "mss32.dll", r"C:\Game\mss32.dll").unwrap();
        assert_eq!(
            e.win.games.impossible_creatures.miles.base, base,
            "the hook went in"
        );
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
                    e.mem
                        .write(dst + 4 * i, 4, i.wrapping_mul(0x0101_0101))
                        .unwrap();
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
            assert_eq!(
                run(&mut e),
                native,
                "step {step:#x}: identical mix, state and registers"
            );
            crate::hooks::apply(&mut e, "mss32", base);
        }
    }
}
