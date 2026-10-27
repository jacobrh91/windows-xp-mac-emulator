//! x87 floating-point unit. Registers are `f64` rather than 80-bit extended; that is
//! enough for game code (and what most emulators aimed at games do).

use crate::cpu::{CF, EAX, Emu, Op, PF, Prefix, Res, Stop, ZF};

const C0: u16 = 0x0100;
const C2: u16 = 0x0400;
const C3: u16 = 0x4000;
const CC_MASK: u16 = 0x4700;

/// `repr(C)` with the register stack first, for compiled code (`jit`).
#[derive(Clone)]
#[repr(C)]
pub struct Fpu {
    pub(crate) st: [f64; 8],
    pub(crate) top: usize,
    /// Condition code bits C0..C3 as they appear in the status word.
    pub(crate) cc: u16,
    pub cw: u16,
    /// MMX registers (kept apart from `st`: well-behaved code never mixes the two without `emms`).
    pub mm: [u64; 8],
    /// SSE registers (four f32 lanes each, as raw bits) and the SSE control/status word.
    pub xmm: [[u32; 4]; 8],
    pub mxcsr: u32,
}

impl Fpu {
    pub fn new() -> Self {
        Fpu {
            st: [0.0; 8],
            top: 0,
            cc: 0,
            cw: 0x037F,
            mm: [0; 8],
            xmm: [[0; 4]; 8],
            mxcsr: 0x1F80,
        }
    }

    fn get(&self, i: usize) -> f64 {
        self.st[(self.top + i) & 7]
    }

    fn set(&mut self, i: usize, v: f64) {
        self.st[(self.top + i) & 7] = v;
    }

    fn push(&mut self, v: f64) {
        self.top = (self.top.wrapping_sub(1)) & 7;
        self.st[self.top] = v;
    }

    fn pop(&mut self) -> f64 {
        let v = self.st[self.top];
        self.top = (self.top + 1) & 7;
        v
    }

    pub fn status_word(&self) -> u16 {
        (self.cc & CC_MASK) | ((self.top as u16) << 11)
    }

    fn compare(&mut self, a: f64, b: f64) {
        self.cc = if a.is_nan() || b.is_nan() {
            C3 | C2 | C0
        } else if a > b {
            0
        } else if a < b {
            C0
        } else {
            C3
        };
    }

    fn round(&self, v: f64) -> f64 {
        match (self.cw >> 10) & 3 {
            0 => v.round_ties_even(),
            1 => v.floor(),
            2 => v.ceil(),
            _ => v.trunc(),
        }
    }

    /// Round per the control word and fit into `bits` (16/32/64); out of range gives the
    /// hardware's "integer indefinite" value.
    fn to_int(&self, v: f64, bits: u32) -> u64 {
        let r = self.round(v);
        let (lo, hi) = match bits {
            16 => (-32768.0, 32767.0),
            32 => (-2147483648.0, 2147483647.0),
            _ => (-9223372036854775808.0, 9223372036854774784.0),
        };
        if r.is_nan() || r < lo || r > hi {
            return 1u64 << (bits - 1);
        }
        r as i64 as u64
    }

    /// The result of an add, subtract, multiply, divide or square root as the precision control
    /// (control word bits 8-9) leaves it: 00 rounds to single precision (24-bit mantissa).
    /// That is the setting Direct3D gives a program, so its arithmetic is all single precision.
    /// (Extended precision is computed as double.) Rounding the double result again to single
    /// gives the correctly rounded single result for these operations.
    pub(crate) fn rounded(&self, v: f64) -> f64 {
        if (self.cw >> 8) & 3 == 0 {
            v as f32 as f64
        } else {
            v
        }
    }

    /// op: 0 add, 1 mul, 4 sub (a-b), 5 subr (b-a), 6 div (a/b), 7 divr (b/a).
    fn arith(op: usize, a: f64, b: f64) -> f64 {
        match op {
            0 => a + b,
            1 => a * b,
            4 => a - b,
            5 => b - a,
            6 => a / b,
            _ => b / a,
        }
    }
}

fn from_m80(bytes: &[u8]) -> f64 {
    let mant = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let se = u16::from_le_bytes([bytes[8], bytes[9]]);
    let sign = if se & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (se & 0x7fff) as i32;
    if exp == 0x7fff {
        return if mant << 1 == 0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        };
    }
    if mant == 0 {
        return sign * 0.0;
    }
    // Scale in two steps: a single powi() overflows internally below 2^-1022.
    let e = exp - 16383 - 63;
    sign * (mant as f64) * 2f64.powi(e / 2) * 2f64.powi(e - e / 2)
}

fn to_m80(v: f64) -> [u8; 10] {
    let mut out = [0u8; 10];
    let sign: u16 = if v.is_sign_negative() { 0x8000 } else { 0 };
    let (mant, exp): (u64, u16) = if v.is_nan() {
        (0xC000_0000_0000_0000, 0x7fff)
    } else if v.is_infinite() {
        (0x8000_0000_0000_0000, 0x7fff)
    } else if v == 0.0 || v.abs() < f64::MIN_POSITIVE {
        (0, 0)
    } else {
        let bits = v.abs().to_bits();
        let e = ((bits >> 52) & 0x7ff) as i32 - 1023;
        let frac = bits & ((1 << 52) - 1);
        (((1u64 << 52) | frac) << 11, (e + 16383) as u16)
    };
    out[0..8].copy_from_slice(&mant.to_le_bytes());
    out[8..10].copy_from_slice(&(sign | exp).to_le_bytes());
    out
}

impl Emu {
    pub(crate) fn exec_fpu(&mut self, opc: u8, p: Prefix, start: u32) -> Res<()> {
        let m = self.modrm(&p)?;
        let group = (opc - 0xD8) as usize;
        match m.rm {
            Op::Mem(addr) => self.fpu_mem(group, m.reg, addr, start),
            Op::Reg(i) => self.fpu_reg(group, m.reg, i, start),
        }
    }

    fn fpu_binary_with(&mut self, op: usize, b: f64) {
        match op {
            2 | 3 => {
                let a = self.fpu.get(0);
                self.fpu.compare(a, b);
                if op == 3 {
                    self.fpu.pop();
                }
            }
            _ => {
                let a = self.fpu.get(0);
                let r = self.fpu.rounded(Fpu::arith(op, a, b));
                self.fpu.set(0, r);
            }
        }
    }

    pub(crate) fn fpu_mem(&mut self, group: usize, op: usize, addr: u32, start: u32) -> Res<()> {
        match (group, op) {
            // arithmetic with a memory operand: float32 / float64 / int32 / int16
            (0, _) => {
                let b = f32::from_bits(self.mem.read(addr, 4)?) as f64;
                self.fpu_binary_with(op, b);
            }
            (4, _) => {
                let b = self.read_f64(addr)?;
                self.fpu_binary_with(op, b);
            }
            (2, _) => {
                let b = self.mem.read(addr, 4)? as i32 as f64;
                self.fpu_binary_with(op, b);
            }
            (6, _) => {
                let b = self.mem.read(addr, 2)? as u16 as i16 as f64;
                self.fpu_binary_with(op, b);
            }
            // D9: float32 load/store, control word
            (1, 0) => {
                let v = f32::from_bits(self.mem.read(addr, 4)?) as f64;
                self.fpu.push(v);
            }
            (1, 2) | (1, 3) => {
                let v = self.fpu.get(0) as f32;
                self.mem.write(addr, 4, v.to_bits())?;
                if op == 3 {
                    self.fpu.pop();
                }
            }
            (1, 5) => self.fpu.cw = self.mem.read(addr, 2)? as u16,
            (1, 7) => self.mem.write(addr, 2, self.fpu.cw as u32)?,
            // DB: int32 load/store, float80
            (3, 0) => {
                let v = self.mem.read(addr, 4)? as i32 as f64;
                self.fpu.push(v);
            }
            (3, 2) | (3, 3) => {
                let v = self.fpu.to_int(self.fpu.get(0), 32) as u32;
                self.mem.write(addr, 4, v)?;
                if op == 3 {
                    self.fpu.pop();
                }
            }
            (3, 5) => {
                let bytes = self.mem.read_bytes(addr, 10)?;
                self.fpu.push(from_m80(&bytes));
            }
            (3, 7) => {
                let bytes = to_m80(self.fpu.get(0));
                self.mem.write_bytes(addr, &bytes)?;
                self.fpu.pop();
            }
            // DD: float64 load/store, status word
            (5, 0) => {
                let v = self.read_f64(addr)?;
                self.fpu.push(v);
            }
            (5, 2) | (5, 3) => {
                let bits = self.fpu.get(0).to_bits();
                self.mem.write(addr, 4, bits as u32)?;
                self.mem.write(addr + 4, 4, (bits >> 32) as u32)?;
                if op == 3 {
                    self.fpu.pop();
                }
            }
            (5, 7) => self.mem.write(addr, 2, self.fpu.status_word() as u32)?,
            // DF: int16 / int64
            (7, 0) => {
                let v = self.mem.read(addr, 2)? as u16 as i16 as f64;
                self.fpu.push(v);
            }
            (7, 2) | (7, 3) => {
                let v = self.fpu.to_int(self.fpu.get(0), 16) as u32;
                self.mem.write(addr, 2, v)?;
                if op == 3 {
                    self.fpu.pop();
                }
            }
            (7, 5) => {
                let lo = self.mem.read(addr, 4)? as u64;
                let hi = self.mem.read(addr + 4, 4)? as u64;
                self.fpu.push(((hi << 32) | lo) as i64 as f64);
            }
            (7, 7) => {
                let v = self.fpu.to_int(self.fpu.get(0), 64);
                self.mem.write(addr, 4, v as u32)?;
                self.mem.write(addr + 4, 4, (v >> 32) as u32)?;
                self.fpu.pop();
            }
            _ => return Err(self.invalid(start)),
        }
        Ok(())
    }

    fn read_f64(&self, addr: u32) -> Res<f64> {
        let lo = self.mem.read(addr, 4)? as u64;
        let hi = self.mem.read(addr + 4, 4)? as u64;
        Ok(f64::from_bits((hi << 32) | lo))
    }

    /// fcomi/fucomi: results land in ZF/PF/CF.
    fn fpu_compare_flags(&mut self, i: usize) {
        let (a, b) = (self.fpu.get(0), self.fpu.get(i));
        let (zf, pf, cf) = if a.is_nan() || b.is_nan() {
            (true, true, true)
        } else {
            (a == b, false, a < b)
        };
        self.eflags &= !(ZF | PF | CF | 0x800 | 0x80 | 0x10);
        self.eflags |= (zf as u32 * ZF) | (pf as u32 * PF) | (cf as u32 * CF);
    }

    pub(crate) fn fpu_reg(&mut self, group: usize, op: usize, i: usize, start: u32) -> Res<()> {
        match (group, op) {
            // D8: st0 = st0 op sti
            (0, _) => {
                let b = self.fpu.get(i);
                self.fpu_binary_with(op, b);
            }
            // DC / DE: sti = sti op st0 (sub/div encodings are swapped vs. D8), DE pops
            (4, _) | (6, _) => {
                if group == 6 && op == 3 {
                    if i != 1 {
                        return Err(self.invalid(start));
                    }
                    let (a, b) = (self.fpu.get(0), self.fpu.get(1));
                    self.fpu.compare(a, b);
                    self.fpu.pop();
                    self.fpu.pop();
                    return Ok(());
                }
                if op == 2 || op == 3 {
                    return Err(self.invalid(start));
                }
                let eff = match op {
                    4 => 5,
                    5 => 4,
                    6 => 7,
                    7 => 6,
                    o => o,
                };
                let r = self
                    .fpu
                    .rounded(Fpu::arith(eff, self.fpu.get(i), self.fpu.get(0)));
                self.fpu.set(i, r);
                if group == 6 {
                    self.fpu.pop();
                }
            }
            (2, 5) if i == 1 => {
                // fucompp
                let (a, b) = (self.fpu.get(0), self.fpu.get(1));
                self.fpu.compare(a, b);
                self.fpu.pop();
                self.fpu.pop();
            }
            (1, 0) => {
                let v = self.fpu.get(i);
                self.fpu.push(v);
            }
            (1, 1) => {
                let (a, b) = (self.fpu.get(0), self.fpu.get(i));
                self.fpu.set(0, b);
                self.fpu.set(i, a);
            }
            (1, 2) if i == 0 => {}
            (1, 4) => {
                let v = self.fpu.get(0);
                match i {
                    0 => self.fpu.set(0, -v),
                    1 => self.fpu.set(0, v.abs()),
                    4 => self.fpu.compare(v, 0.0),
                    5 => {
                        // fxam: classify st0 (C1 is the sign). Empty slots are not tracked,
                        // so every register examines as holding its value.
                        let class = if v.is_nan() {
                            C0
                        } else if v.is_infinite() {
                            C2 | C0
                        } else if v == 0.0 {
                            C3
                        } else if v.is_subnormal() {
                            C3 | C2
                        } else {
                            C2
                        };
                        self.fpu.cc = class | if v.is_sign_negative() { 0x0200 } else { 0 };
                    }
                    _ => return Err(self.invalid(start)),
                }
            }
            (1, 5) => {
                let c = match i {
                    0 => 1.0,
                    1 => 10f64.log2(),
                    2 => std::f64::consts::LOG2_E,
                    3 => std::f64::consts::PI,
                    4 => 2f64.log10(),
                    5 => std::f64::consts::LN_2,
                    6 => 0.0,
                    _ => return Err(self.invalid(start)),
                };
                self.fpu.push(c);
            }
            (1, 6) => {
                let x = self.fpu.get(0);
                match i {
                    0 => self.fpu.set(0, x.exp2() - 1.0),
                    1 => {
                        let y = self.fpu.get(1);
                        self.fpu.set(1, y * x.log2());
                        self.fpu.pop();
                    }
                    2 => {
                        self.fpu.set(0, x.tan());
                        self.fpu.push(1.0);
                        self.fpu.cc &= !C2;
                    }
                    3 => {
                        let y = self.fpu.get(1);
                        self.fpu.set(1, y.atan2(x));
                        self.fpu.pop();
                    }
                    5 => {
                        let y = self.fpu.get(1);
                        let q = (x / y).round_ties_even();
                        self.fpu.set(0, x - q * y);
                        self.fpu.cc &= !C2;
                    }
                    6 => self.fpu.top = (self.fpu.top.wrapping_sub(1)) & 7,
                    7 => self.fpu.top = (self.fpu.top + 1) & 7,
                    _ => return Err(self.invalid(start)),
                }
            }
            (1, 7) => {
                let x = self.fpu.get(0);
                match i {
                    0 => {
                        let y = self.fpu.get(1);
                        self.fpu.set(0, x % y);
                        self.fpu.cc &= !C2;
                    }
                    1 => {
                        let y = self.fpu.get(1);
                        self.fpu.set(1, y * (x + 1.0).log2());
                        self.fpu.pop();
                    }
                    2 => self.fpu.set(0, self.fpu.rounded(x.sqrt())),
                    3 => {
                        self.fpu.set(0, x.sin());
                        self.fpu.push(x.cos());
                        self.fpu.cc &= !C2;
                    }
                    4 => {
                        let r = self.fpu.round(x);
                        self.fpu.set(0, r);
                    }
                    5 => {
                        let y = self.fpu.get(1).trunc();
                        self.fpu.set(0, x * y.exp2());
                    }
                    6 => {
                        self.fpu.set(0, x.sin());
                        self.fpu.cc &= !C2;
                    }
                    _ => {
                        self.fpu.set(0, x.cos());
                        self.fpu.cc &= !C2;
                    }
                }
            }
            (3, 4) => match i {
                2 => self.fpu.cc = 0,       // fnclex
                3 => self.fpu = Fpu::new(), // fninit
                _ => return Err(self.invalid(start)),
            },
            (3, 5) | (3, 6) => self.fpu_compare_flags(i),
            (5, 0) => {} // ffree
            (5, 2) | (5, 3) => {
                let v = self.fpu.get(0);
                self.fpu.set(i, v);
                if op == 3 {
                    self.fpu.pop();
                }
            }
            (5, 4) | (5, 5) => {
                let (a, b) = (self.fpu.get(0), self.fpu.get(i));
                self.fpu.compare(a, b);
                if op == 5 {
                    self.fpu.pop();
                }
            }
            (7, 4) if i == 0 => {
                let sw = self.fpu.status_word() as u32;
                self.regs[EAX] = (self.regs[EAX] & 0xffff_0000) | sw;
            }
            (7, 5) | (7, 6) => {
                self.fpu_compare_flags(i);
                self.fpu.pop();
            }
            _ => return Err(self.invalid(start)),
        }
        Ok(())
    }
}

#[allow(dead_code)]
fn _unused(_: Stop) {}

#[cfg(test)]
mod tests;
