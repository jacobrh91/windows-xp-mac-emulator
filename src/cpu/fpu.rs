//! x87 floating-point unit. Registers are `f64` rather than 80-bit extended; that is
//! enough for game code (and what most emulators aimed at games do).

use crate::cpu::{Emu, Op, Prefix, Res, Stop, CF, EAX, PF, ZF};

const C0: u16 = 0x0100;
const C2: u16 = 0x0400;
const C3: u16 = 0x4000;
const CC_MASK: u16 = 0x4700;

#[derive(Clone)]
pub struct Fpu {
    st: [f64; 8],
    top: usize,
    /// Condition code bits C0..C3 as they appear in the status word.
    cc: u16,
    pub cw: u16,
    /// MMX registers (kept apart from `st`: well-behaved code never mixes the two without `emms`).
    pub mm: [u64; 8],
    /// SSE registers (four f32 lanes each, as raw bits) and the SSE control/status word.
    pub xmm: [[u32; 4]; 8],
    pub mxcsr: u32,
}

impl Fpu {
    pub fn new() -> Self {
        Fpu { st: [0.0; 8], top: 0, cc: 0, cw: 0x037F, mm: [0; 8], xmm: [[0; 4]; 8], mxcsr: 0x1F80 }
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
        return if mant << 1 == 0 { sign * f64::INFINITY } else { f64::NAN };
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
                self.fpu.set(0, Fpu::arith(op, a, b));
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
                let r = Fpu::arith(eff, self.fpu.get(i), self.fpu.get(0));
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
                    2 => self.fpu.set(0, x.sqrt()),
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
                2 => self.fpu.cc = 0, // fnclex
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
mod tests {
    use crate::cpu::{Emu, Stop, CF, EAX, ZF};

    const CODE: u32 = 0x1000;
    const A: u32 = 0x2000;
    const B: u32 = 0x2100;
    const OUT: u32 = 0x2200;

    fn abs(op: &[u8], addr: u32) -> Vec<u8> {
        let mut v = op.to_vec();
        v.extend_from_slice(&addr.to_le_bytes());
        v
    }

    /// Assemble `chunks`, append hlt, run.
    fn run(setup: impl FnOnce(&mut Emu), chunks: &[Vec<u8>]) -> Emu {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x10000);
        setup(&mut e);
        let mut code: Vec<u8> = chunks.concat();
        code.push(0xF4);
        e.mem.write_bytes(CODE, &code).unwrap();
        e.eip = CODE;
        assert_eq!(e.run(10_000), Stop::Halt);
        e
    }

    fn write_f64(e: &mut Emu, addr: u32, v: f64) {
        let b = v.to_bits();
        e.mem.write(addr, 4, b as u32).unwrap();
        e.mem.write(addr + 4, 4, (b >> 32) as u32).unwrap();
    }

    fn read_f64(e: &Emu, addr: u32) -> f64 {
        let lo = e.mem.read(addr, 4).unwrap() as u64;
        let hi = e.mem.read(addr + 4, 4).unwrap() as u64;
        f64::from_bits((hi << 32) | lo)
    }

    #[test]
    fn one_plus_one_is_two() {
        let e = run(
            |_| {},
            &[vec![0xD9, 0xE8, 0xD9, 0xE8, 0xDE, 0xC1], abs(&[0xDB, 0x1D], OUT)], // fld1 fld1 faddp fistp
        );
        assert_eq!(e.mem.read(OUT, 4).unwrap(), 2);
    }

    #[test]
    fn float32_multiply_stored_as_float64() {
        let e = run(
            |e| {
                e.mem.write(A, 4, 1.5f32.to_bits()).unwrap();
                e.mem.write(B, 4, 2.0f32.to_bits()).unwrap();
            },
            &[abs(&[0xD9, 0x05], A), abs(&[0xD8, 0x0D], B), abs(&[0xDD, 0x1D], OUT)],
        );
        assert_eq!(read_f64(&e, OUT), 3.0);
    }

    #[test]
    fn sub_and_div_register_forms_have_correct_operand_order() {
        // st1 = 10, st0 = 3; fsubp st1,st0 -> 7
        let e = run(
            |e| {
                e.mem.write(A, 4, 10).unwrap();
                e.mem.write(B, 4, 3).unwrap();
            },
            &[
                abs(&[0xDB, 0x05], A), // fild [A]
                abs(&[0xDB, 0x05], B), // fild [B]
                vec![0xDE, 0xE9],      // fsubp st1, st0
                abs(&[0xDB, 0x1D], OUT),
            ],
        );
        assert_eq!(e.mem.read(OUT, 4).unwrap(), 7);

        // 12 / 4 via fdivp st1, st0 (DE F9)
        let e = run(
            |e| {
                e.mem.write(A, 4, 12).unwrap();
                e.mem.write(B, 4, 4).unwrap();
            },
            &[
                abs(&[0xDB, 0x05], A),
                abs(&[0xDB, 0x05], B),
                vec![0xDE, 0xF9],
                abs(&[0xDB, 0x1D], OUT),
            ],
        );
        assert_eq!(e.mem.read(OUT, 4).unwrap(), 3);
    }

    #[test]
    fn fcomip_sets_cpu_flags() {
        // fld1; fldz; fcomip st0,st1 (0 < 1 -> CF); fstp st0
        let e = run(|_| {}, &[vec![0xD9, 0xE8, 0xD9, 0xEE, 0xDF, 0xF1, 0xDD, 0xD8]]);
        assert!(e.eflags & CF != 0);
        assert!(e.eflags & ZF == 0);
        // equal
        let e = run(|_| {}, &[vec![0xD9, 0xE8, 0xD9, 0xE8, 0xDF, 0xF1, 0xDD, 0xD8]]);
        assert!(e.eflags & ZF != 0 && e.eflags & CF == 0);
    }

    #[test]
    fn fnstsw_sahf_branch_idiom() {
        // fld1 (st1=1); fldz (st0=0); fcompp -> st0 < st1 -> C0; fnstsw ax; sahf -> CF
        let e = run(|_| {}, &[vec![0xD9, 0xE8, 0xD9, 0xEE, 0xDE, 0xD9, 0xDF, 0xE0, 0x9E]]);
        assert!(e.eflags & CF != 0);
        assert_eq!(e.regs[EAX] & 0x4500, 0x0100);
    }

    #[test]
    fn sqrt_of_integer() {
        let e = run(
            |e| e.mem.write(A, 4, 16).unwrap(),
            &[abs(&[0xDB, 0x05], A), vec![0xD9, 0xFA], abs(&[0xDB, 0x1D], OUT)],
        );
        assert_eq!(e.mem.read(OUT, 4).unwrap(), 4);
    }

    #[test]
    fn fist_honours_rounding_control() {
        let prog = |cw: u16| {
            run(
                |e| {
                    write_f64(e, A, 2.7);
                    e.mem.write(B, 2, cw as u32).unwrap();
                },
                &[abs(&[0xD9, 0x2D], B), abs(&[0xDD, 0x05], A), abs(&[0xDB, 0x1D], OUT)],
            )
        };
        assert_eq!(prog(0x037F).mem.read(OUT, 4).unwrap(), 3, "round to nearest");
        assert_eq!(prog(0x0F7F).mem.read(OUT, 4).unwrap(), 2, "truncate");
    }

    #[test]
    fn nearest_rounds_ties_to_even() {
        let e = run(
            |e| write_f64(e, A, 2.5),
            &[abs(&[0xDD, 0x05], A), abs(&[0xDB, 0x1D], OUT)],
        );
        assert_eq!(e.mem.read(OUT, 4).unwrap(), 2);
    }

    #[test]
    fn out_of_range_gives_integer_indefinite() {
        let e = run(
            |e| write_f64(e, A, 1e20),
            &[abs(&[0xDD, 0x05], A), abs(&[0xDB, 0x1D], OUT)],
        );
        assert_eq!(e.mem.read(OUT, 4).unwrap(), 0x8000_0000);
    }

    #[test]
    fn extended_precision_roundtrip() {
        for v in [std::f64::consts::PI, -0.5, 1e300, 1e-300, 0.0, 123456789.0] {
            let e = run(
                |e| write_f64(e, A, v),
                &[
                    abs(&[0xDD, 0x05], A),  // fld qword [A]
                    abs(&[0xDB, 0x3D], B),  // fstp tbyte [B]
                    abs(&[0xDB, 0x2D], B),  // fld tbyte [B]
                    abs(&[0xDD, 0x1D], OUT), // fstp qword [OUT]
                ],
            );
            assert_eq!(read_f64(&e, OUT), v, "{v}");
        }
    }

    #[test]
    fn constants_and_trig() {
        // fldpi; fcos -> -1
        let e = run(|_| {}, &[vec![0xD9, 0xEB, 0xD9, 0xFF], abs(&[0xDD, 0x1D], OUT)]);
        assert!((read_f64(&e, OUT) + 1.0).abs() < 1e-12);
        // fld1 fld1 fpatan -> pi/4
        let e = run(|_| {}, &[vec![0xD9, 0xE8, 0xD9, 0xE8, 0xD9, 0xF3], abs(&[0xDD, 0x1D], OUT)]);
        assert!((read_f64(&e, OUT) - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
    }

    #[test]
    fn fxam_classifies_st0() {
        use super::{C2, C3, CC_MASK};
        // fld1; fchs; fxam; fnstsw ax  -> normal (C2) and negative (C1)
        let e = run(|_| {}, &[vec![0xD9, 0xE8, 0xD9, 0xE0, 0xD9, 0xE5, 0xDF, 0xE0]]);
        assert_eq!(e.regs[EAX] as u16 & CC_MASK, C2 | 0x0200);
        // fldz; fxam -> zero (C3), positive
        let e = run(|_| {}, &[vec![0xD9, 0xEE, 0xD9, 0xE5, 0xDF, 0xE0]]);
        assert_eq!(e.regs[EAX] as u16 & CC_MASK, C3);
    }

    #[test]
    fn fxch_fchs_fabs() {
        // fld1; fldz; fxch -> st0 = 1; fchs -> -1; fabs -> 1 ; store
        let e = run(
            |_| {},
            &[vec![0xD9, 0xE8, 0xD9, 0xEE, 0xD9, 0xC9, 0xD9, 0xE0, 0xD9, 0xE1], abs(&[0xDD, 0x1D], OUT)],
        );
        assert_eq!(read_f64(&e, OUT), 1.0);
    }

    #[test]
    fn fild_int64_and_fistp_int64() {
        let e = run(
            |e| {
                e.mem.write(A, 4, 0).unwrap();
                e.mem.write(A + 4, 4, 1).unwrap(); // 1 << 32
            },
            &[abs(&[0xDF, 0x2D], A), abs(&[0xDF, 0x3D], OUT)],
        );
        assert_eq!(e.mem.read(OUT + 4, 4).unwrap(), 1);
        assert_eq!(e.mem.read(OUT, 4).unwrap(), 0);
    }

    #[test]
    fn fwait_before_fnstcw() {
        // fwait; fnstcw [OUT]  (the sequence compilers emit for fstcw)
        let e = run(|_| {}, &[vec![0x9B], abs(&[0xD9, 0x3D], OUT)]);
        assert_eq!(e.mem.read(OUT, 2).unwrap(), 0x037F, "default control word");
    }

    #[test]
    fn stack_wraps_after_push_pop() {
        let e = run(|_| {}, &[[0xD9, 0xE8].repeat(3), [0xDD, 0xD8].repeat(3)]);
        assert_eq!(e.fpu.status_word() >> 11, 0, "balanced push/pop returns TOP to 0");
    }
}
