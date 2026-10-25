//! SSE: the Pentium III's 128-bit single-precision SIMD instructions.
//!
//! Eight XMM registers of four `f32` lanes, kept as raw bits so moves, shuffles and the
//! bitwise operations are exact. Arithmetic is done lane by lane in `f32`, which is what the
//! hardware does too. Only the single-precision (SSE1) instruction set is offered; the CPU
//! does not claim SSE2, so the 0x66-prefixed double-precision forms are only accepted where
//! they are bit-for-bit the same operation (moves and the logical ops).

use crate::cpu::{CF, Emu, OF, Op, PF, Prefix, Res, SF, ZF};

/// Four lanes of raw bits.
pub type Xmm = [u32; 4];

fn f(v: u32) -> f32 {
    f32::from_bits(v)
}

fn lanes(a: Xmm, b: Xmm, op: impl Fn(f32, f32) -> f32) -> Xmm {
    [0, 1, 2, 3].map(|i| op(f(a[i]), f(b[i])).to_bits())
}

/// The arithmetic in 0F 51..=5F: (op, a, b) -> result for one lane.
fn arith(opc: u8, a: f32, b: f32) -> f32 {
    match opc {
        0x51 => b.sqrt(),
        0x52 => 1.0 / b.sqrt(),
        0x53 => 1.0 / b,
        0x58 => a + b,
        0x59 => a * b,
        0x5C => a - b,
        0x5E => a / b,
        // min/max return the second operand when either is NaN (or both are zero).
        0x5D => {
            if a < b {
                a
            } else {
                b
            }
        }
        _ => {
            if a > b {
                a
            } else {
                b
            }
        }
    }
}

/// CMPPS/CMPSS predicate: all ones when true.
fn compare(pred: u8, a: f32, b: f32) -> u32 {
    let unordered = a.is_nan() || b.is_nan();
    let r = match pred & 7 {
        0 => a == b,
        1 => a < b,
        2 => a <= b,
        3 => unordered,
        4 => a != b || unordered,
        5 => a >= b || unordered,
        6 => a > b || unordered,
        _ => !unordered,
    };
    if r { u32::MAX } else { 0 }
}

impl Emu {
    fn read_xmm(&self, op: Op) -> Res<Xmm> {
        Ok(match op {
            Op::Reg(r) => self.fpu.xmm[r & 7],
            Op::Mem(a) => {
                let (lo, hi) = (self.mem.read64(a)?, self.mem.read64(a.wrapping_add(8))?);
                [lo as u32, (lo >> 32) as u32, hi as u32, (hi >> 32) as u32]
            }
        })
    }

    fn write_xmm_mem(&mut self, a: u32, v: Xmm) -> Res<()> {
        self.mem.write64(a, v[0] as u64 | (v[1] as u64) << 32)?;
        self.mem
            .write64(a.wrapping_add(8), v[2] as u64 | (v[3] as u64) << 32)?;
        Ok(())
    }

    /// A scalar source: the low lane of a register, or 32 bits of memory.
    fn read_scalar(&self, op: Op) -> Res<u32> {
        Ok(match op {
            Op::Reg(r) => self.fpu.xmm[r & 7][0],
            Op::Mem(a) => self.mem.read(a, 4)?,
        })
    }

    /// Execute the 0F-prefixed SSE opcode `opc`. Ok(false): not an SSE instruction.
    pub(crate) fn exec_sse(&mut self, opc: u8, p: &Prefix) -> Res<bool> {
        let scalar = p.rep == 0xF3;
        if p.rep == 0xF2 {
            return Ok(false); // F2-prefixed forms are SSE2 (double precision)
        }
        match opc {
            // movups / movss (10 load, 11 store), movaps (28 load, 29 store), movntps (2B)
            0x10 | 0x28 => {
                let m = self.modrm(p)?;
                let r = m.reg & 7;
                if scalar && opc == 0x10 {
                    let v = self.read_scalar(m.rm)?;
                    match m.rm {
                        Op::Reg(_) => self.fpu.xmm[r][0] = v,
                        Op::Mem(_) => self.fpu.xmm[r] = [v, 0, 0, 0], // a load clears the rest
                    }
                } else {
                    self.fpu.xmm[r] = self.read_xmm(m.rm)?;
                }
            }
            0x11 | 0x29 | 0x2B => {
                let m = self.modrm(p)?;
                let v = self.fpu.xmm[m.reg & 7];
                match (m.rm, scalar && opc == 0x11) {
                    (Op::Reg(r), true) => self.fpu.xmm[r & 7][0] = v[0],
                    (Op::Mem(a), true) => self.mem.write(a, 4, v[0])?,
                    (Op::Reg(r), false) => self.fpu.xmm[r & 7] = v,
                    (Op::Mem(a), false) => self.write_xmm_mem(a, v)?,
                }
            }
            // movlps / movhlps (12), movlps store (13), movhps / movlhps (16), movhps store (17)
            0x12 | 0x16 => {
                let m = self.modrm(p)?;
                let r = m.reg & 7;
                let half = match m.rm {
                    Op::Reg(s) => {
                        let s = self.fpu.xmm[s & 7];
                        if opc == 0x12 {
                            [s[2], s[3]]
                        } else {
                            [s[0], s[1]]
                        } // movhlps / movlhps
                    }
                    Op::Mem(a) => [self.mem.read(a, 4)?, self.mem.read(a.wrapping_add(4), 4)?],
                };
                let at = if opc == 0x12 { 0 } else { 2 };
                self.fpu.xmm[r][at..at + 2].copy_from_slice(&half);
            }
            0x13 | 0x17 => {
                let m = self.modrm(p)?;
                let v = self.fpu.xmm[m.reg & 7];
                let Op::Mem(a) = m.rm else { return Ok(false) };
                let at = if opc == 0x13 { 0 } else { 2 };
                self.mem
                    .write64(a, v[at] as u64 | (v[at + 1] as u64) << 32)?;
            }
            // unpcklps / unpckhps
            0x14 | 0x15 => {
                let m = self.modrm(p)?;
                let (d, s) = (self.fpu.xmm[m.reg & 7], self.read_xmm(m.rm)?);
                self.fpu.xmm[m.reg & 7] = if opc == 0x14 {
                    [d[0], s[0], d[1], s[1]]
                } else {
                    [d[2], s[2], d[3], s[3]]
                };
            }
            // cvtpi2ps xmm, mm/m64 ; cvtsi2ss xmm, r/m32
            0x2A => {
                let m = self.modrm(p)?;
                let r = m.reg & 7;
                if scalar {
                    let v = self.read_op(m.rm, 32)? as i32;
                    self.fpu.xmm[r][0] = (v as f32).to_bits();
                } else {
                    let v = match m.rm {
                        Op::Reg(s) => self.fpu.mm[s & 7],
                        Op::Mem(a) => self.mem.read64(a)?,
                    };
                    self.fpu.xmm[r][0] = (v as u32 as i32 as f32).to_bits();
                    self.fpu.xmm[r][1] = ((v >> 32) as u32 as i32 as f32).to_bits();
                }
            }
            // cvttss2si / cvtss2si r32 ; cvttps2pi / cvtps2pi mm
            0x2C | 0x2D => {
                let m = self.modrm(p)?;
                let to_int = |x: f32| -> u32 {
                    let x = if opc == 0x2C {
                        x.trunc()
                    } else {
                        x.round_ties_even()
                    };
                    if x.is_nan() || !(-2147483648.0..2147483648.0).contains(&x) {
                        0x8000_0000
                    } else {
                        x as i32 as u32
                    }
                };
                if scalar {
                    let v = f(self.read_scalar(m.rm)?);
                    self.set_reg32(m.reg, to_int(v));
                } else {
                    let v = self.read_xmm(m.rm)?;
                    self.fpu.mm[m.reg & 7] =
                        to_int(f(v[0])) as u64 | (to_int(f(v[1])) as u64) << 32;
                }
            }
            // ucomiss / comiss: compare low lanes into ZF, PF, CF
            0x2E | 0x2F => {
                let m = self.modrm(p)?;
                let (a, b) = (f(self.fpu.xmm[m.reg & 7][0]), f(self.read_scalar(m.rm)?));
                let bits = if a.is_nan() || b.is_nan() {
                    ZF | PF | CF
                } else if a < b {
                    CF
                } else if a == b {
                    ZF
                } else {
                    0
                };
                self.eflags = (self.eflags & !(ZF | PF | CF | OF | SF | 0x10)) | bits;
            }
            // movmskps r32, xmm: the four sign bits
            0x50 => {
                let m = self.modrm(p)?;
                let Op::Reg(s) = m.rm else { return Ok(false) };
                let v = self.fpu.xmm[s & 7];
                let mask = (0..4).fold(0, |acc, i| acc | ((v[i] >> 31) << i));
                self.set_reg32(m.reg, mask);
            }
            // andps, andnps, orps, xorps (and their 66-prefixed pd twins: the same bits)
            0x54..=0x57 => {
                let m = self.modrm(p)?;
                let (d, s) = (self.fpu.xmm[m.reg & 7], self.read_xmm(m.rm)?);
                self.fpu.xmm[m.reg & 7] = [0, 1, 2, 3].map(|i| match opc {
                    0x54 => d[i] & s[i],
                    0x55 => !d[i] & s[i],
                    0x56 => d[i] | s[i],
                    _ => d[i] ^ s[i],
                });
            }
            // arithmetic: sqrt, rsqrt, rcp, add, mul, sub, min, div, max
            0x51..=0x53 | 0x58 | 0x59 | 0x5C..=0x5F => {
                if p.opsize16 {
                    return Ok(false); // double-precision (SSE2)
                }
                let m = self.modrm(p)?;
                let r = m.reg & 7;
                if scalar {
                    let (a, b) = (f(self.fpu.xmm[r][0]), f(self.read_scalar(m.rm)?));
                    self.fpu.xmm[r][0] = arith(opc, a, b).to_bits();
                } else {
                    let s = self.read_xmm(m.rm)?;
                    self.fpu.xmm[r] = lanes(self.fpu.xmm[r], s, |a, b| arith(opc, a, b));
                }
            }
            // cmpps / cmpss with a predicate byte
            0xC2 => {
                let m = self.modrm(p)?;
                let pred = self.fetch8()?;
                let r = m.reg & 7;
                if scalar {
                    let (a, b) = (f(self.fpu.xmm[r][0]), f(self.read_scalar(m.rm)?));
                    self.fpu.xmm[r][0] = compare(pred, a, b);
                } else {
                    let (d, s) = (self.fpu.xmm[r], self.read_xmm(m.rm)?);
                    self.fpu.xmm[r] = [0, 1, 2, 3].map(|i| compare(pred, f(d[i]), f(s[i])));
                }
            }
            // shufps: two lanes from the destination, two from the source
            0xC6 => {
                let m = self.modrm(p)?;
                let imm = self.fetch8()? as usize;
                let (d, s) = (self.fpu.xmm[m.reg & 7], self.read_xmm(m.rm)?);
                self.fpu.xmm[m.reg & 7] = [
                    d[imm & 3],
                    d[(imm >> 2) & 3],
                    s[(imm >> 4) & 3],
                    s[(imm >> 6) & 3],
                ];
            }
            // ldmxcsr / stmxcsr, and the fences (which order nothing on one CPU)
            0xAE => {
                let m = self.modrm(p)?;
                match (m.rm, m.reg) {
                    (Op::Mem(a), 2) => self.fpu.mxcsr = self.mem.read(a, 4)?,
                    (Op::Mem(a), 3) => self.mem.write(a, 4, self.fpu.mxcsr)?,
                    (Op::Reg(_), 5..=7) => {}
                    _ => return Ok(false),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn set_reg32(&mut self, r: usize, v: u32) {
        self.regs[r & 7] = v;
    }
}

/// Opcodes (after 0F) handled by `exec_sse`.
pub fn is_sse(opc: u8) -> bool {
    matches!(opc, 0x10..=0x17 | 0x28..=0x2F | 0x50..=0x5F | 0xC2 | 0xC6 | 0xAE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run machine code (0x1000) with XMM registers preset; return the emulator.
    fn run(code: &[u8], setup: impl FnOnce(&mut Emu)) -> Emu {
        let mut e = Emu::new();
        e.mem.map(0x1000, 0x10000);
        e.mem.write_bytes(0x1000, code).unwrap();
        e.mem
            .write_bytes(0x1000 + code.len() as u32, &[0xF4])
            .unwrap(); // hlt
        e.eip = 0x1000;
        e.regs[crate::cpu::ESP] = 0x10000;
        setup(&mut e);
        e.run(1000);
        e
    }

    fn xmm(v: [f32; 4]) -> Xmm {
        v.map(f32::to_bits)
    }

    fn get(e: &Emu, r: usize) -> [f32; 4] {
        e.fpu.xmm[r].map(f32::from_bits)
    }

    #[test]
    fn packed_arithmetic() {
        // addps xmm0, xmm1 ; mulps xmm0, xmm1
        let e = run(&[0x0F, 0x58, 0xC1, 0x0F, 0x59, 0xC1], |e| {
            e.fpu.xmm[0] = xmm([1.0, 2.0, 3.0, 4.0]);
            e.fpu.xmm[1] = xmm([10.0, 20.0, 30.0, 40.0]);
        });
        assert_eq!(get(&e, 0), [110.0, 440.0, 990.0, 1760.0]);
    }

    #[test]
    fn scalar_forms_touch_only_the_low_lane() {
        // addss xmm0, xmm1 ; rsqrtps xmm2, xmm1 (packed)
        let e = run(&[0xF3, 0x0F, 0x58, 0xC1, 0x0F, 0x52, 0xD1], |e| {
            e.fpu.xmm[0] = xmm([1.0, 2.0, 3.0, 4.0]);
            e.fpu.xmm[1] = xmm([4.0, 16.0, 64.0, 0.25]);
        });
        assert_eq!(get(&e, 0), [5.0, 2.0, 3.0, 4.0]);
        assert_eq!(get(&e, 2), [0.5, 0.25, 0.125, 2.0]);
    }

    #[test]
    fn shufps_and_unpack() {
        // shufps xmm0, xmm1, 0b00_01_10_11 -> [d3, d2, s1, s0] ; unpcklps xmm2, xmm1
        let e = run(&[0x0F, 0xC6, 0xC1, 0x1B, 0x0F, 0x14, 0xD1], |e| {
            e.fpu.xmm[0] = xmm([0.0, 1.0, 2.0, 3.0]);
            e.fpu.xmm[1] = xmm([10.0, 11.0, 12.0, 13.0]);
            e.fpu.xmm[2] = xmm([20.0, 21.0, 22.0, 23.0]);
        });
        assert_eq!(get(&e, 0), [3.0, 2.0, 11.0, 10.0]);
        assert_eq!(get(&e, 2), [20.0, 10.0, 21.0, 11.0]);
    }

    #[test]
    fn loads_and_stores() {
        // movaps xmm0, [0x2000] ; movss xmm1, [0x2004] ; movups [0x3000], xmm0 ; movss [0x3010], xmm1
        let code = [
            0x0F, 0x28, 0x05, 0x00, 0x20, 0, 0, //
            0xF3, 0x0F, 0x10, 0x0D, 0x04, 0x20, 0, 0, //
            0x0F, 0x11, 0x05, 0x00, 0x30, 0, 0, //
            0xF3, 0x0F, 0x11, 0x0D, 0x10, 0x30, 0, 0,
        ];
        let e = run(&code, |e| {
            for (i, v) in [1.5f32, 2.5, 3.5, 4.5].iter().enumerate() {
                e.mem.write(0x2000 + 4 * i as u32, 4, v.to_bits()).unwrap();
            }
            e.fpu.xmm[1] = xmm([9.0; 4]);
        });
        assert_eq!(get(&e, 0), [1.5, 2.5, 3.5, 4.5]);
        assert_eq!(
            get(&e, 1),
            [2.5, 0.0, 0.0, 0.0],
            "a scalar load clears the upper lanes"
        );
        assert_eq!(f32::from_bits(e.mem.read(0x300C, 4).unwrap()), 4.5);
        assert_eq!(f32::from_bits(e.mem.read(0x3010, 4).unwrap()), 2.5);
    }

    #[test]
    fn logic_compare_and_conversions() {
        // xorps xmm0, xmm0 ; cmpps xmm1, xmm2, 1 (lt) ; movmskps eax, xmm1 ;
        // cvttss2si ecx, xmm3 ; comiss xmm3, xmm2
        let code = [
            0x0F, 0x57, 0xC0, 0x0F, 0xC2, 0xCA, 0x01, 0x0F, 0x50, 0xC1, 0xF3, 0x0F, 0x2C, 0xCB,
            0x0F, 0x2F, 0xDA,
        ];
        let e = run(&code, |e| {
            e.fpu.xmm[0] = xmm([1.0; 4]);
            e.fpu.xmm[1] = xmm([1.0, 5.0, 1.0, 5.0]);
            e.fpu.xmm[2] = xmm([2.0; 4]);
            e.fpu.xmm[3] = xmm([-7.9, 0.0, 0.0, 0.0]);
        });
        assert_eq!(e.fpu.xmm[0], [0; 4]);
        assert_eq!(
            e.regs[crate::cpu::EAX],
            0b0101,
            "lanes 0 and 2 were less than 2"
        );
        assert_eq!(e.regs[crate::cpu::ECX] as i32, -7, "truncates toward zero");
        assert_ne!(e.eflags & CF, 0, "-7.9 < 2.0 sets CF");
        assert_eq!(e.eflags & ZF, 0);
    }

    #[test]
    fn mxcsr_round_trips() {
        // ldmxcsr [0x2000] ; stmxcsr [0x2004]
        let e = run(
            &[
                0x0F, 0xAE, 0x15, 0x00, 0x20, 0, 0, 0x0F, 0xAE, 0x1D, 0x04, 0x20, 0, 0,
            ],
            |e| {
                e.mem.write(0x2000, 4, 0x1F80).unwrap();
            },
        );
        assert_eq!(e.mem.read(0x2004, 4).unwrap(), 0x1F80);
    }
}
