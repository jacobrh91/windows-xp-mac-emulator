//! MMX: the 64-bit integer SIMD instructions of the Pentium MMX / Pentium II era.
//!
//! Operands are packed lanes inside a `u64`. Each helper works lane by lane on bytes, words or
//! dwords, which keeps the semantics (wraparound, saturation, signedness) easy to check.

use crate::cpu::{Emu, Op, Prefix, Res};

fn bytes(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

fn words(v: u64) -> [u16; 4] {
    [
        v as u16,
        (v >> 16) as u16,
        (v >> 32) as u16,
        (v >> 48) as u16,
    ]
}

fn dwords(v: u64) -> [u32; 2] {
    [v as u32, (v >> 32) as u32]
}

fn from_words(w: [u16; 4]) -> u64 {
    w[0] as u64 | (w[1] as u64) << 16 | (w[2] as u64) << 32 | (w[3] as u64) << 48
}

fn from_dwords(d: [u32; 2]) -> u64 {
    d[0] as u64 | (d[1] as u64) << 32
}

fn map8(a: u64, b: u64, f: impl Fn(u8, u8) -> u8) -> u64 {
    let (x, y) = (bytes(a), bytes(b));
    let mut o = [0u8; 8];
    for i in 0..8 {
        o[i] = f(x[i], y[i]);
    }
    u64::from_le_bytes(o)
}

fn map16(a: u64, b: u64, f: impl Fn(u16, u16) -> u16) -> u64 {
    let (x, y) = (words(a), words(b));
    from_words([f(x[0], y[0]), f(x[1], y[1]), f(x[2], y[2]), f(x[3], y[3])])
}

fn map32(a: u64, b: u64, f: impl Fn(u32, u32) -> u32) -> u64 {
    let (x, y) = (dwords(a), dwords(b));
    from_dwords([f(x[0], y[0]), f(x[1], y[1])])
}

fn sat_i8(v: i32) -> u8 {
    v.clamp(-128, 127) as i8 as u8
}

fn sat_i16(v: i32) -> u16 {
    v.clamp(-32768, 32767) as i16 as u16
}

fn sat_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

fn sat_u16(v: i32) -> u16 {
    v.clamp(0, 65535) as u16
}

fn mask8(c: bool) -> u8 {
    if c { 0xFF } else { 0 }
}

fn mask16(c: bool) -> u16 {
    if c { 0xFFFF } else { 0 }
}

fn mask32(c: bool) -> u32 {
    if c { u32::MAX } else { 0 }
}

/// Shift every lane of `width` bits by `count` (counts past the lane width clear it, or fill
/// with the sign for arithmetic right shifts).
fn shift(v: u64, count: u64, width: u32, kind: char) -> u64 {
    let lanes = 64 / width;
    let mut out = 0u64;
    let lane_mask = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    for i in 0..lanes {
        let x = (v >> (i * width)) & lane_mask;
        let r = match kind {
            'l' => {
                if count >= width as u64 {
                    0
                } else {
                    (x << count) & lane_mask
                }
            }
            'r' => {
                if count >= width as u64 {
                    0
                } else {
                    x >> count
                }
            }
            _ => {
                let signed = ((x << (64 - width)) as i64) >> (64 - width);
                let c = count.min(width as u64 - 1);
                ((signed >> c) as u64) & lane_mask
            }
        };
        out |= r << (i * width);
    }
    out
}

/// Lane-wise add without carries crossing lanes; `high` has each lane's top bit set.
#[inline(always)]
fn swar_add(a: u64, b: u64, high: u64) -> u64 {
    ((a & !high).wrapping_add(b & !high)) ^ ((a ^ b) & high)
}

#[inline(always)]
fn swar_sub(a: u64, b: u64, high: u64) -> u64 {
    ((a | high).wrapping_sub(b & !high)) ^ ((a ^ !b) & high)
}

/// Opcodes `binary` implements.
fn is_binary(opc: u8) -> bool {
    matches!(opc, 0xFC..=0xFE | 0xF8..=0xFA | 0xEC | 0xED | 0xE8 | 0xE9 | 0xDC | 0xDD | 0xD8 | 0xD9 | 0xD5 | 0xE5 | 0xF5 | 0xDB | 0xDF | 0xEB | 0xEF | 0x74..=0x76 | 0x64..=0x66 | 0x63 | 0x67 | 0x6B | 0x60..=0x62 | 0x68..=0x6A | 0xD1..=0xD3 | 0xE1 | 0xE2 | 0xF1..=0xF3)
}

/// The two-operand register/memory forms: `dst = op(dst, src)`. None if `opc` is not one.
#[inline(always)]
fn binary(opc: u8, d: u64, s: u64) -> Option<u64> {
    Some(match opc {
        0xFC => swar_add(d, s, 0x8080_8080_8080_8080),
        0xFD => swar_add(d, s, 0x8000_8000_8000_8000),
        0xFE => swar_add(d, s, 0x8000_0000_8000_0000),
        0xF8 => swar_sub(d, s, 0x8080_8080_8080_8080),
        0xF9 => swar_sub(d, s, 0x8000_8000_8000_8000),
        0xFA => swar_sub(d, s, 0x8000_0000_8000_0000),
        0xEC => map8(d, s, |a, b| sat_i8(a as i8 as i32 + b as i8 as i32)),
        0xED => map16(d, s, |a, b| sat_i16(a as i16 as i32 + b as i16 as i32)),
        0xE8 => map8(d, s, |a, b| sat_i8(a as i8 as i32 - b as i8 as i32)),
        0xE9 => map16(d, s, |a, b| sat_i16(a as i16 as i32 - b as i16 as i32)),
        0xDC => map8(d, s, |a, b| sat_u8(a as i32 + b as i32)),
        0xDD => map16(d, s, |a, b| sat_u16(a as i32 + b as i32)),
        0xD8 => map8(d, s, |a, b| sat_u8(a as i32 - b as i32)),
        0xD9 => map16(d, s, |a, b| sat_u16(a as i32 - b as i32)),
        0xD5 => map16(d, s, |a, b| (a as i16 as i32 * b as i16 as i32) as u16),
        0xE5 => map16(d, s, |a, b| {
            ((a as i16 as i32 * b as i16 as i32) >> 16) as u16
        }),
        0xF5 => {
            let (x, y) = (words(d), words(s));
            let p = |i: usize| x[i] as i16 as i32 * y[i] as i16 as i32;
            from_dwords([(p(0) + p(1)) as u32, (p(2) + p(3)) as u32])
        }
        0xDB => d & s,
        0xDF => !d & s,
        0xEB => d | s,
        0xEF => d ^ s,
        0x74 => map8(d, s, |a, b| mask8(a == b)),
        0x75 => map16(d, s, |a, b| mask16(a == b)),
        0x76 => map32(d, s, |a, b| mask32(a == b)),
        0x64 => map8(d, s, |a, b| mask8(a as i8 > b as i8)),
        0x65 => map16(d, s, |a, b| mask16(a as i16 > b as i16)),
        0x66 => map32(d, s, |a, b| mask32(a as i32 > b as i32)),
        0x63 => {
            // packsswb: words of dst then of src, saturated to signed bytes
            let (x, y) = (words(d), words(s));
            let mut o = [0u8; 8];
            for i in 0..4 {
                o[i] = sat_i8(x[i] as i16 as i32);
                o[4 + i] = sat_i8(y[i] as i16 as i32);
            }
            u64::from_le_bytes(o)
        }
        0x67 => {
            let (x, y) = (words(d), words(s));
            let mut o = [0u8; 8];
            for i in 0..4 {
                o[i] = sat_u8(x[i] as i16 as i32);
                o[4 + i] = sat_u8(y[i] as i16 as i32);
            }
            u64::from_le_bytes(o)
        }
        0x6B => {
            let (x, y) = (dwords(d), dwords(s));
            from_words([
                sat_i16(x[0] as i32),
                sat_i16(x[1] as i32),
                sat_i16(y[0] as i32),
                sat_i16(y[1] as i32),
            ])
        }
        0x60 => {
            let (x, y) = (bytes(d), bytes(s));
            u64::from_le_bytes([x[0], y[0], x[1], y[1], x[2], y[2], x[3], y[3]])
        }
        0x68 => {
            let (x, y) = (bytes(d), bytes(s));
            u64::from_le_bytes([x[4], y[4], x[5], y[5], x[6], y[6], x[7], y[7]])
        }
        0x61 => {
            let (x, y) = (words(d), words(s));
            from_words([x[0], y[0], x[1], y[1]])
        }
        0x69 => {
            let (x, y) = (words(d), words(s));
            from_words([x[2], y[2], x[3], y[3]])
        }
        0x62 => from_dwords([dwords(d)[0], dwords(s)[0]]),
        0x6A => from_dwords([dwords(d)[1], dwords(s)[1]]),
        // shifts by a count in the second operand
        0xD1 => shift(d, s, 16, 'r'),
        0xD2 => shift(d, s, 32, 'r'),
        0xD3 => shift(d, s, 64, 'r'),
        0xE1 => shift(d, s, 16, 'a'),
        0xE2 => shift(d, s, 32, 'a'),
        0xF1 => shift(d, s, 16, 'l'),
        0xF2 => shift(d, s, 32, 'l'),
        0xF3 => shift(d, s, 64, 'l'),
        _ => return None,
    })
}

impl Emu {
    /// Execute the 0F-prefixed MMX opcode `opc` (already fetched). Returns Ok(false) if it is
    /// not an MMX instruction.
    #[inline(always)]
    pub(crate) fn exec_mmx(&mut self, opc: u8, p: &Prefix) -> Res<bool> {
        match opc {
            0x77 => {} // emms: nothing to reset, MMX state is separate from the x87 stack
            0x6E => {
                let m = self.modrm(p)?;
                let v = self.read_op(m.rm, 32)?;
                self.fpu.mm[m.reg & 7] = v as u64;
            }
            0x7E => {
                let m = self.modrm(p)?;
                let v = self.fpu.mm[m.reg & 7] as u32;
                self.write_op(m.rm, 32, v)?;
            }
            0x6F | 0x7F => {
                let m = self.modrm(p)?;
                if opc == 0x6F {
                    self.fpu.mm[m.reg & 7] = self.read_mm(m.rm)?;
                } else {
                    let v = self.fpu.mm[m.reg & 7];
                    match m.rm {
                        Op::Reg(r) => self.fpu.mm[r & 7] = v,
                        Op::Mem(a) => self.mem.write64(a, v)?,
                    }
                }
            }
            // shift by immediate: groups 71 (words), 72 (dwords), 73 (qwords)
            0x71..=0x73 => {
                let m = self.modrm(p)?;
                let count = self.fetch8()? as u64;
                let Op::Reg(r) = m.rm else { return Ok(false) };
                let (width, kind) = match (opc, m.reg) {
                    (0x71, 2) => (16, 'r'),
                    (0x71, 4) => (16, 'a'),
                    (0x71, 6) => (16, 'l'),
                    (0x72, 2) => (32, 'r'),
                    (0x72, 4) => (32, 'a'),
                    (0x72, 6) => (32, 'l'),
                    (0x73, 2) => (64, 'r'),
                    (0x73, 6) => (64, 'l'),
                    _ => return Ok(false),
                };
                self.fpu.mm[r & 7] = shift(self.fpu.mm[r & 7], count, width, kind);
            }
            _ => {
                if !is_binary(opc) {
                    return Ok(false);
                }
                let m = self.modrm(p)?;
                let src = self.read_mm(m.rm)?;
                let i = m.reg & 7;
                self.fpu.mm[i] = binary(opc, self.fpu.mm[i], src).unwrap_or(0);
            }
        }
        Ok(true)
    }

    /// A 64-bit MMX source operand: an MMX register or 8 bytes of memory.
    fn read_mm(&self, op: Op) -> Res<u64> {
        Ok(match op {
            Op::Reg(r) => self.fpu.mm[r & 7],
            Op::Mem(a) => self.mem.read64(a)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraparound_and_saturating_arithmetic() {
        assert_eq!(
            binary(0xFC, 0x00FF_00FF_00FF_00FF, 0x0001_0001_0001_0001),
            Some(0),
            "paddb adds bytewise: no carry leaks into the next lane"
        );
        assert_eq!(
            binary(0xFC, 0xFF, 0x01),
            Some(0),
            "255 + 1 wraps to 0 in the low byte"
        );
        assert_eq!(
            binary(0xDC, 0xFF, 0x01),
            Some(0xFF),
            "paddusb saturates at 255"
        );
        assert_eq!(binary(0xD8, 0x01, 0x02), Some(0), "psubusb saturates at 0");
        assert_eq!(
            binary(0xED, 0x7FFF, 0x0001),
            Some(0x7FFF),
            "paddsw saturates at 32767"
        );
        assert_eq!(
            binary(0xE9, 0x8000, 0x0001),
            Some(0x8000),
            "psubsw saturates at -32768"
        );
    }

    #[test]
    fn swar_matches_lane_arithmetic() {
        let samples = [
            0u64,
            u64::MAX,
            0x0123_4567_89AB_CDEF,
            0xFF00_FF00_FF00_FF00,
            0x8000_7FFF_0001_FFFF,
            0x7F80_807F_0102_FEFF,
        ];
        for &a in &samples {
            for &b in &samples {
                assert_eq!(
                    swar_add(a, b, 0x8080_8080_8080_8080),
                    map8(a, b, |x, y| x.wrapping_add(y))
                );
                assert_eq!(
                    swar_add(a, b, 0x8000_8000_8000_8000),
                    map16(a, b, |x, y| x.wrapping_add(y))
                );
                assert_eq!(
                    swar_add(a, b, 0x8000_0000_8000_0000),
                    map32(a, b, |x, y| x.wrapping_add(y))
                );
                assert_eq!(
                    swar_sub(a, b, 0x8080_8080_8080_8080),
                    map8(a, b, |x, y| x.wrapping_sub(y))
                );
                assert_eq!(
                    swar_sub(a, b, 0x8000_8000_8000_8000),
                    map16(a, b, |x, y| x.wrapping_sub(y))
                );
                assert_eq!(
                    swar_sub(a, b, 0x8000_0000_8000_0000),
                    map32(a, b, |x, y| x.wrapping_sub(y))
                );
            }
        }
    }

    #[test]
    fn every_binary_opcode_is_declared() {
        for opc in 0..=255u8 {
            assert_eq!(
                is_binary(opc),
                binary(opc, 0, 0).is_some(),
                "opcode {opc:#x}"
            );
        }
    }

    #[test]
    fn multiplies() {
        assert_eq!(
            binary(0xD5, 0x0003_0002, 0x0004_0005).map(|v| v & 0xFFFF_FFFF),
            Some(0x000C_000A),
            "pmullw low halves"
        );
        assert_eq!(
            binary(0xE5, 0x4000, 0x0004),
            Some(1),
            "pmulhw high half of 0x4000*4 = 0x10000"
        );
        // pmaddwd: (1*5 + 2*6, 3*7 + 4*8)
        let a = from_words([1, 2, 3, 4]);
        let b = from_words([5, 6, 7, 8]);
        assert_eq!(binary(0xF5, a, b), Some(from_dwords([17, 53])));
    }

    #[test]
    fn compares_and_logic() {
        assert_eq!(
            binary(0x74, 0x0102, 0x0102),
            Some(u64::MAX),
            "every byte lane equal"
        );
        assert_eq!(
            binary(0x74, 0x0102, 0x0103),
            Some(!0xFF),
            "only the low byte differs"
        );
        assert_eq!(binary(0x64, 5, 3).unwrap() & 0xFF, 0xFF, "pcmpgtb: 5 > 3");
        assert_eq!(
            binary(0x64, 0x80, 0x01).unwrap() & 0xFF,
            0,
            "pcmpgtb is signed: -128 > 1 is false"
        );
        assert_eq!(binary(0xDF, 0xF0, 0xFF), Some(0x0F), "pandn: !dst & src");
        assert_eq!(binary(0xEF, 0xAA, 0xFF), Some(0x55), "pxor");
    }

    #[test]
    fn packing_and_unpacking() {
        let a = from_words([0x0100, 0xFFFF, 0x0005, 0x8000]);
        let r = binary(0x67, a, 0).unwrap();
        assert_eq!(bytes(r)[..4], [255, 0, 5, 0], "packuswb: clamps to 0..255");
        let r = binary(0x63, a, 0).unwrap();
        assert_eq!(
            bytes(r)[..4],
            [127, 0xFF, 5, 0x80],
            "packsswb: clamps to -128..127"
        );
        assert_eq!(
            binary(0x60, 0x0403_0201, 0x0807_0605),
            Some(0x0804_0703_0602_0501),
            "punpcklbw interleaves the low bytes"
        );
        assert_eq!(
            binary(0x62, from_dwords([1, 2]), from_dwords([3, 4])),
            Some(from_dwords([1, 3])),
            "punpckldq"
        );
        assert_eq!(
            binary(0x6A, from_dwords([1, 2]), from_dwords([3, 4])),
            Some(from_dwords([2, 4])),
            "punpckhdq"
        );
    }

    #[test]
    fn shifts() {
        assert_eq!(
            shift(from_words([1, 2, 3, 4]), 1, 16, 'l'),
            from_words([2, 4, 6, 8])
        );
        assert_eq!(
            shift(from_words([0x8000, 2, 0, 0]), 1, 16, 'r'),
            from_words([0x4000, 1, 0, 0])
        );
        assert_eq!(
            shift(from_words([0x8000, 0, 0, 0]), 1, 16, 'a'),
            from_words([0xC000, 0, 0, 0]),
            "psraw keeps the sign"
        );
        assert_eq!(
            shift(from_words([1, 1, 1, 1]), 16, 16, 'l'),
            0,
            "counts >= lane width clear the lane"
        );
        assert_eq!(
            shift(from_words([0x8000, 0, 0, 0]), 99, 16, 'a'),
            from_words([0xFFFF, 0, 0, 0]),
            "psraw with a huge count fills with the sign"
        );
        assert_eq!(shift(1, 63, 64, 'l'), 1 << 63);
        assert_eq!(shift(1 << 63, 63, 64, 'r'), 1);
    }
}
