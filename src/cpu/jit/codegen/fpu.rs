//! x87: the register stack lives in `emu.fpu` (x16 = &st[0], x17 = top).

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};

use super::*;

fn fpu_base(a: &mut Asm) {
    dynasm!(a ; .arch aarch64 ; add x16, x28, #OFF_FPU ; ldr x17, [x16, #64]);
}

/// d0 = st(i)
fn fget(a: &mut Asm, i: u32, d: u8) {
    dynasm!(a ; .arch aarch64 ; add x9, x17, #i ; and x9, x9, #7);
    match d {
        0 => dynasm!(a ; .arch aarch64 ; ldr d0, [x16, x9, lsl #3]),
        _ => dynasm!(a ; .arch aarch64 ; ldr d1, [x16, x9, lsl #3]),
    }
}

/// st(i) = d0
fn fset(a: &mut Asm, i: u32) {
    dynasm!(a ; .arch aarch64 ; add x9, x17, #i ; and x9, x9, #7 ; str d0, [x16, x9, lsl #3]);
}

/// push d0
fn fpush(a: &mut Asm) {
    dynasm!(a ; .arch aarch64
        ; sub x17, x17, #1
        ; and x17, x17, #7
        ; str d0, [x16, x17, lsl #3]
        ; str x17, [x16, #64]
    );
}

fn fpop(a: &mut Asm) {
    dynasm!(a ; .arch aarch64 ; add x17, x17, #1 ; and x17, x17, #7 ; str x17, [x16, #64]);
}

/// d0 = arith(op, d0, d1) as `Fpu::arith` (0 add, 1 mul, 4 sub, 5 subr, 6 div, 7 divr).
fn farith(a: &mut Asm, op: u8) {
    match op {
        0 => dynasm!(a ; .arch aarch64 ; fadd d0, d0, d1),
        1 => dynasm!(a ; .arch aarch64 ; fmul d0, d0, d1),
        4 => dynasm!(a ; .arch aarch64 ; fsub d0, d0, d1),
        5 => dynasm!(a ; .arch aarch64 ; fsub d0, d1, d0),
        6 => dynasm!(a ; .arch aarch64 ; fdiv d0, d0, d1),
        _ => dynasm!(a ; .arch aarch64 ; fdiv d0, d1, d0),
    }
    fround_precision(a);
}

/// d0 rounded as the precision control asks (`Fpu::rounded`): single precision when
/// control word bits 8-9 are 00. x16 = &emu.fpu.
fn fround_precision(a: &mut Asm) {
    dynasm!(a ; .arch aarch64
        ; ldrh w9, [x16, #OFF_FPU_CW]
        ; tst w9, #0x300
        ; b.ne >full
        ; fcvt s0, d0
        ; fcvt d0, s0
        ; full:
    );
}

/// Compare d0 (st0) with d1 into the condition codes as `Fpu::compare` does:
/// unordered C3|C2|C0, less C0, equal C3, greater none.
fn fcompare(a: &mut Asm) {
    dynasm!(a ; .arch aarch64
        ; fcmp d0, d1
        ; movz w1, #0
        ; movz w2, #0x100
        ; movz w3, #0x4000
        ; movz w4, #0x4500
        ; csel w1, w2, w1, mi
        ; csel w1, w3, w1, eq
        ; csel w1, w4, w1, vs
        ; strh w1, [x16, #72]
    );
}

/// x1 = st0 (d0) rounded per the control word's RC and converted to a `bits` (32 or 64)
/// integer, out of range or NaN giving the "integer indefinite" value, as `Fpu::to_int`.
fn fround_int(a: &mut Asm, bits: u32) {
    dynasm!(a ; .arch aarch64
        ; ldrh w2, [x16, #OFF_FPU_CW]
        ; ubfx w2, w2, #10, #2
        ; frintn d1, d0
        ; frintm d2, d0
        ; frintp d3, d0
        ; frintz d4, d0
        ; cmp w2, #1
        ; fcsel d1, d2, d1, eq
        ; cmp w2, #2
        ; fcsel d1, d3, d1, eq
        ; cmp w2, #3
        ; fcsel d1, d4, d1, eq
        ; fcvtzs x1, d1
        ; movz x2, #0x8000, lsl #48
    );
    if bits == 32 {
        dynasm!(a ; .arch aarch64
            ; movz w2, #0x8000, lsl #16
            ; cmp x1, w1, sxtw
            ; csel w1, w2, w1, ne
            ; fcmp d1, d1
            ; csel w1, w2, w1, vs
        );
    } else {
        // fcvtzs saturates: only an overflow gives i64::MAX.
        dynasm!(a ; .arch aarch64
            ; movn x3, #0x8000, lsl #48
            ; cmp x1, x3
            ; csel x1, x2, x1, eq
            ; fcmp d1, d1
            ; csel x1, x2, x1, vs
        );
    }
}

/// The common x87 instructions. false: leave it to the interpreter.
pub(super) fn fpu(a: &mut Asm, opc: u8, reg: u8, rm: Ea, slow: DynamicLabel) -> bool {
    let arith = |o: u8| matches!(o, 0 | 1 | 4..=7);
    match (opc, reg, rm) {
        // fld m32 / m64
        (
            0xD9,
            0,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        )
        | (
            0xDD,
            0,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) => {
            ea(a, base, index, scale, disp);
            if opc == 0xD9 {
                load(a, 4, 1, slow);
                dynasm!(a ; .arch aarch64 ; fmov s0, w1 ; fcvt d0, s0);
            } else {
                load64(a, slow);
                dynasm!(a ; .arch aarch64 ; fmov d0, x1);
            }
            fpu_base(a);
            fpush(a);
        }
        // fst / fstp m32 / m64
        (
            0xD9 | 0xDD,
            2 | 3,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) => {
            fpu_base(a);
            fget(a, 0, 0);
            ea(a, base, index, scale, disp);
            if opc == 0xD9 {
                dynasm!(a ; .arch aarch64 ; fcvt s0, d0 ; fmov w1, s0);
                store32(a, 1, slow);
            } else {
                dynasm!(a ; .arch aarch64 ; fmov x1, d0);
                store64(a, slow);
            }
            if reg == 3 {
                fpop(a);
            }
        }
        // st0 = st0 op m32 / m64
        (
            0xD8 | 0xDC,
            o,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) if arith(o) => {
            ea(a, base, index, scale, disp);
            if opc == 0xD8 {
                load(a, 4, 1, slow);
                dynasm!(a ; .arch aarch64 ; fmov s1, w1 ; fcvt d1, s1);
            } else {
                load64(a, slow);
                dynasm!(a ; .arch aarch64 ; fmov d1, x1);
            }
            fpu_base(a);
            fget(a, 0, 0);
            farith(a, o);
            fset(a, 0);
        }
        // st0 = st0 op st(i)
        (0xD8, o, Ea::Reg(i)) if arith(o) => {
            fpu_base(a);
            fget(a, 0, 0);
            fget(a, i as u32, 1);
            farith(a, o);
            fset(a, 0);
        }
        // st(i) = st(i) op st0, sub/div encodings swapped; DE pops
        (0xDC | 0xDE, o, Ea::Reg(i)) if arith(o) => {
            let eff = match o {
                4 => 5,
                5 => 4,
                6 => 7,
                7 => 6,
                o => o,
            };
            fpu_base(a);
            fget(a, i as u32, 0);
            fget(a, 0, 1);
            farith(a, eff);
            fset(a, i as u32);
            if opc == 0xDE {
                fpop(a);
            }
        }
        // fld st(i)
        (0xD9, 0, Ea::Reg(i)) => {
            fpu_base(a);
            fget(a, i as u32, 0);
            fpush(a);
        }
        // fxch st(i)
        (0xD9, 1, Ea::Reg(i)) => {
            fpu_base(a);
            fget(a, 0, 0);
            fget(a, i as u32, 1);
            dynasm!(a ; .arch aarch64 ; fmov d2, d0 ; fmov d0, d1);
            fset(a, 0);
            dynasm!(a ; .arch aarch64 ; fmov d0, d2);
            fset(a, i as u32);
        }
        // fst / fstp st(i)
        (0xDD, 2 | 3, Ea::Reg(i)) => {
            fpu_base(a);
            fget(a, 0, 0);
            fset(a, i as u32);
            if reg == 3 {
                fpop(a);
            }
        }
        // fchs, fabs
        (0xD9, 4, Ea::Reg(i @ (0 | 1))) => {
            fpu_base(a);
            fget(a, 0, 0);
            if i == 0 {
                dynasm!(a ; .arch aarch64 ; fneg d0, d0);
            } else {
                dynasm!(a ; .arch aarch64 ; fabs d0, d0);
            }
            fset(a, 0);
        }
        // fcom / fcomp with m32 / m64 / st(i); fcompp
        (
            0xD8 | 0xDC,
            2 | 3,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) => {
            ea(a, base, index, scale, disp);
            if opc == 0xD8 {
                load(a, 4, 1, slow);
                dynasm!(a ; .arch aarch64 ; fmov s1, w1 ; fcvt d1, s1);
            } else {
                load64(a, slow);
                dynasm!(a ; .arch aarch64 ; fmov d1, x1);
            }
            fpu_base(a);
            fget(a, 0, 0);
            fcompare(a);
            if reg == 3 {
                fpop(a);
            }
        }
        (0xD8, 2 | 3, Ea::Reg(i)) => {
            fpu_base(a);
            fget(a, 0, 0);
            fget(a, i as u32, 1);
            fcompare(a);
            if reg == 3 {
                fpop(a);
            }
        }
        (0xDE, 3, Ea::Reg(1)) => {
            fpu_base(a);
            fget(a, 0, 0);
            fget(a, 1, 1);
            fcompare(a);
            fpop(a);
            fpop(a);
        }
        // fnstsw ax: AX = C3..C0 | top << 11
        (0xDF, 4, Ea::Reg(0)) => {
            fpu_base(a);
            dynasm!(a ; .arch aarch64
                ; ldrh w1, [x16, #72]
                ; movz w2, #0x4700
                ; and w1, w1, w2
                ; orr w1, w1, w17, lsl #11
                ; bfi W(g(0)), w1, #0, #16
            );
        }
        // fild m32 / m16
        (
            0xDB | 0xDF,
            0,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) => {
            ea(a, base, index, scale, disp);
            if opc == 0xDB {
                load(a, 4, 1, slow);
            } else {
                load(a, 2, 1, slow);
                dynasm!(a ; .arch aarch64 ; sxth w1, w1);
            }
            dynasm!(a ; .arch aarch64 ; scvtf d0, w1);
            fpu_base(a);
            fpush(a);
        }
        // fild m64
        (
            0xDF,
            5,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) => {
            ea(a, base, index, scale, disp);
            load64(a, slow);
            dynasm!(a ; .arch aarch64 ; scvtf d0, x1);
            fpu_base(a);
            fpush(a);
        }
        // fist / fistp m32, fistp m64
        (
            0xDB | 0xDF,
            r,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) if (opc == 0xDB && matches!(r, 2 | 3)) || (opc == 0xDF && r == 7) => {
            ea(a, base, index, scale, disp);
            fpu_base(a);
            fget(a, 0, 0);
            if opc == 0xDB {
                fround_int(a, 32);
                store32(a, 1, slow);
            } else {
                fround_int(a, 64);
                store64(a, slow);
            }
            if r != 2 {
                fpop(a);
            }
        }
        // st0 = st0 op m32 integer
        (
            0xDA,
            o,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) if arith(o) => {
            ea(a, base, index, scale, disp);
            load(a, 4, 1, slow);
            dynasm!(a ; .arch aarch64 ; scvtf d1, w1);
            fpu_base(a);
            fget(a, 0, 0);
            farith(a, o);
            fset(a, 0);
        }
        // fldcw m16
        (
            0xD9,
            5,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ) => {
            ea(a, base, index, scale, disp);
            load(a, 2, 1, slow);
            fpu_base(a);
            dynasm!(a ; .arch aarch64 ; strh w1, [x16, #OFF_FPU_CW]);
        }
        // fsqrt
        (0xD9, 7, Ea::Reg(2)) => {
            fpu_base(a);
            fget(a, 0, 0);
            dynasm!(a ; .arch aarch64 ; fsqrt d0, d0);
            fround_precision(a);
            fset(a, 0);
        }
        // fld1, fldz
        (0xD9, 5, Ea::Reg(i @ (0 | 6))) => {
            fpu_base(a);
            if i == 0 {
                dynasm!(a ; .arch aarch64 ; fmov d0, 1.0);
            } else {
                dynasm!(a ; .arch aarch64 ; fmov d0, xzr);
            }
            fpush(a);
        }
        _ => return false,
    }
    true
}
