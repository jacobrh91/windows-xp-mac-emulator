//! Flags, ALU operations, shifts and multiplies.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};

use super::*;

/// After an arithmetic/logic instruction set NZCV for result w2: put the new EFLAGS value
/// (CF PF ZF SF OF as `alu` computes them) in w3. `borrow`: CF is ARM's C inverted (sub).
/// `keep_cf`: inc/dec, which leave CF alone.
/// `byte_high`: an 8-bit operation done in the top byte of w2 (see `alu8`).
pub(super) fn flags(a: &mut Asm, borrow: bool, keep_cf: bool) {
    flags_of(a, borrow, keep_cf, false);
}

fn flags_of(a: &mut Asm, borrow: bool, keep_cf: bool, byte_high: bool) {
    if !need_flags() {
        // Overwritten before anything reads them: keep the old flags (w3 = w27).
        dynasm!(a ; .arch aarch64 ; mov w3, w27);
        return;
    }
    if borrow {
        dynasm!(a ; .arch aarch64 ; cset w4, cc);
    } else {
        dynasm!(a ; .arch aarch64 ; cset w4, cs);
    }
    dynasm!(a ; .arch aarch64
        ; cset w5, eq
        ; cset w6, mi
        ; cset w7, vs
    );
    parity(a, byte_high);
    compose(a, keep_cf);
}

/// w8 = PF for the low byte of w2 (or its top byte, for 8-bit operations done up there).
fn parity(a: &mut Asm, byte_high: bool) {
    if !need_flags() {
        return;
    }
    if byte_high {
        dynasm!(a ; .arch aarch64 ; lsr w8, w2, #24);
    } else {
        dynasm!(a ; .arch aarch64 ; and w8, w2, #0xFF);
    }
    dynasm!(a ; .arch aarch64
        ; eor w8, w8, w8, lsr #4
        ; eor w8, w8, w8, lsr #2
        ; eor w8, w8, w8, lsr #1
        ; and w8, w8, #1
        ; eor w8, w8, #1
    );
}

/// w3 = EFLAGS with CF (w4, unless kept), PF (w8), ZF (w5), SF (w6) and OF (w7) replaced.
fn compose(a: &mut Asm, keep_cf: bool) {
    if !need_flags() {
        dynasm!(a ; .arch aarch64 ; mov w3, w27);
        return;
    }
    let clear = if keep_cf {
        ARITH_FLAGS & !1
    } else {
        ARITH_FLAGS
    };
    mov32(a, 9, clear);
    dynasm!(a ; .arch aarch64
        ; bic w3, w27, w9
        ; orr w3, w3, w8, lsl #2
        ; orr w3, w3, w5, lsl #6
        ; orr w3, w3, w6, lsl #7
        ; orr w3, w3, w7, lsl #11
    );
    if !keep_cf {
        dynasm!(a ; .arch aarch64 ; orr w3, w3, w4);
    }
}

/// result w2 = w1 op w4 (op as in `alu`), setting NZCV. None for adc/sbb (need CF in).
pub(super) fn alu(a: &mut Asm, op: u8) -> Option<bool> {
    let borrow = match op {
        0 => {
            dynasm!(a ; .arch aarch64 ; adds w2, w1, w4);
            false
        }
        5 | 7 => {
            dynasm!(a ; .arch aarch64 ; subs w2, w1, w4);
            true
        }
        1 => {
            dynasm!(a ; .arch aarch64 ; orr w2, w1, w4 ; tst w2, w2);
            false
        }
        4 => {
            dynasm!(a ; .arch aarch64 ; ands w2, w1, w4);
            false
        }
        // adc / sbb: set ARM's carry from x86's CF first (sbc borrows when C is clear)
        2 => {
            dynasm!(a ; .arch aarch64 ; and w9, w27, #1 ; cmp w9, #1 ; adcs w2, w1, w4);
            false
        }
        3 => {
            dynasm!(a ; .arch aarch64 ; and w9, w27, #1 ; cmp wzr, w9 ; sbcs w2, w1, w4);
            true
        }
        6 => {
            dynasm!(a ; .arch aarch64 ; eor w2, w1, w4 ; tst w2, w2);
            false
        }
        _ => return None,
    };
    Some(borrow)
}

/// Read an 8-bit operand into W(dst), zero-extended. Registers 4..7 are AH, CH, DH, BH.
pub(super) fn read8(a: &mut Asm, rm: Ea, dst: u8, slow: DynamicLabel) {
    match rm {
        Ea::Reg(r) if r < 4 => dynasm!(a ; .arch aarch64 ; uxtb W(dst), W(g(r))),
        Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; ubfx W(dst), W(g(r - 4)), #8, #8),
        Ea::Mem {
            base,
            index,
            scale,
            disp,
        } => {
            ea(a, base, index, scale, disp);
            load(a, 1, dst, slow);
        }
    }
}

/// The low byte of W(src) into an 8-bit operand (w0 holds a memory operand's address).
pub(super) fn write8(a: &mut Asm, rm: Ea, src: u8, slow: DynamicLabel) {
    match rm {
        Ea::Reg(r) if r < 4 => dynasm!(a ; .arch aarch64 ; bfi W(g(r)), W(src), #0, #8),
        Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; bfi W(g(r - 4)), W(src), #8, #8),
        Ea::Mem { .. } => {
            dynasm!(a ; .arch aarch64
                ; lsr w9, w0, #12
                ; ldrb w12, [x14, x9]
                ; cbnz w12, =>slow
            );
            page(a, 1, slow);
            dynasm!(a ; .arch aarch64 ; strb W(src), [x10, x11]);
        }
    }
}

/// 8-bit `rm = rm op src`: the operands go in the top byte so ARM's flags are x86's.
pub(super) fn alu8(a: &mut Asm, o: u8, rm: Ea, src: AluSrc, test: bool, slow: DynamicLabel) {
    read8(a, rm, 1, slow);
    match src {
        AluSrc::Reg(r) if r < 4 => dynasm!(a ; .arch aarch64 ; uxtb w4, W(g(r))),
        AluSrc::Reg(r) => dynasm!(a ; .arch aarch64 ; ubfx w4, W(g(r - 4)), #8, #8),
        AluSrc::Imm(v) => mov32(a, 4, v & 0xFF),
    }
    dynasm!(a ; .arch aarch64 ; lsl w1, w1, #24 ; lsl w4, w4, #24);
    let borrow = alu(a, o).unwrap();
    flags_of(a, borrow, false, true);
    if o != 7 && !test {
        dynasm!(a ; .arch aarch64 ; lsr w2, w2, #24);
        write8(a, rm, 2, slow);
    }
    dynasm!(a ; .arch aarch64 ; mov w27, w3);
}

/// The 8-bit form of `reg = reg op rm`.
pub(super) fn alu8_reg(a: &mut Asm, o: u8, reg: u8, rm: Ea, slow: DynamicLabel) {
    read8(a, rm, 4, slow);
    read8(a, Ea::Reg(reg), 1, slow);
    dynasm!(a ; .arch aarch64 ; lsl w1, w1, #24 ; lsl w4, w4, #24);
    let borrow = alu(a, o).unwrap();
    flags_of(a, borrow, false, true);
    if o != 7 {
        dynasm!(a ; .arch aarch64 ; lsr w2, w2, #24);
        write8(a, Ea::Reg(reg), 2, slow);
    }
    dynasm!(a ; .arch aarch64 ; mov w27, w3);
}

/// shl/shr/sar by a constant 1..=31, with the interpreter's flag rules.
pub(super) fn shift_imm(a: &mut Asm, op: u8, rm: Ea, c: u32, slow: DynamicLabel) {
    read_rm(a, rm, 1, slow);
    match op {
        4 | 6 => {
            let out = 32 - c;
            dynasm!(a ; .arch aarch64
                ; lsl w2, w1, #c
                ; ubfx w4, w1, #out, #1
                ; lsr w7, w2, #31
                ; eor w7, w7, w4
            );
        }
        5 => {
            let last = c - 1;
            dynasm!(a ; .arch aarch64 ; lsr w2, w1, #c ; ubfx w4, w1, #last, #1 ; lsr w7, w1, #31);
        }
        _ => {
            let last = c - 1;
            dynasm!(a ; .arch aarch64 ; asr w2, w1, #c ; ubfx w4, w1, #last, #1 ; movz w7, #0);
        }
    }
    dynasm!(a ; .arch aarch64 ; cmp w2, #0 ; cset w5, eq ; lsr w6, w2, #31);
    parity(a, false);
    compose(a, false);
    match rm {
        Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov W(g(r)), w2),
        Ea::Mem { .. } => store32(a, 2, slow),
    }
    dynasm!(a ; .arch aarch64 ; mov w27, w3);
}

/// shl/shr/sar by cl (masked to 0..=31). A count of 0 changes nothing, flags included.
pub(super) fn shift_cl(a: &mut Asm, op: u8, rm: Ea, slow: DynamicLabel) {
    read_rm(a, rm, 1, slow);
    dynasm!(a ; .arch aarch64 ; mov w12, W(g(1)) ; and w12, w12, #31 ; cbz w12, >done ; sub w13, w12, #1);
    match op {
        4 | 6 => dynasm!(a ; .arch aarch64
            ; lsl w2, w1, w12
            ; movz w10, #32
            ; sub w10, w10, w12
            ; lsr w4, w1, w10
            ; and w4, w4, #1
            ; lsr w7, w2, #31
            ; eor w7, w7, w4
        ),
        5 => {
            dynasm!(a ; .arch aarch64 ; lsr w2, w1, w12 ; lsr w4, w1, w13 ; and w4, w4, #1 ; lsr w7, w1, #31)
        }
        _ => {
            dynasm!(a ; .arch aarch64 ; asr w2, w1, w12 ; lsr w4, w1, w13 ; and w4, w4, #1 ; movz w7, #0)
        }
    }
    dynasm!(a ; .arch aarch64 ; cmp w2, #0 ; cset w5, eq ; lsr w6, w2, #31);
    parity(a, false);
    compose(a, false);
    match rm {
        Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov W(g(r)), w2),
        Ea::Mem { .. } => store32(a, 2, slow),
    }
    dynasm!(a ; .arch aarch64 ; mov w27, w3 ; done:);
}

/// Signed 32x32 multiply of w1 and w4 into W(g(reg)); CF = OF = "did not fit".
pub(super) fn imul(a: &mut Asm, reg: u8) {
    dynasm!(a ; .arch aarch64
        ; smull x2, w1, w4
        ; sxtw x5, w2
        ; cmp x2, x5
        ; cset w6, ne
    );
    mov32(a, 9, 0x801);
    dynasm!(a ; .arch aarch64
        ; bic w3, w27, w9
        ; orr w3, w3, w6
        ; orr w3, w3, w6, lsl #11
        ; mov W(g(reg)), w2
        ; mov w27, w3
    );
}

/// Read a 32-bit operand into W(dst): a register or memory (w0 holds the address after).
pub(super) fn read_rm(a: &mut Asm, rm: Ea, dst: u8, slow: DynamicLabel) {
    match rm {
        Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov W(dst), W(g(r))),
        Ea::Mem {
            base,
            index,
            scale,
            disp,
        } => {
            ea(a, base, index, scale, disp);
            load(a, 4, dst, slow);
        }
    }
}

pub(super) enum AluSrc {
    Reg(u8),
    Imm(u32),
}

/// `rm = rm op src` (or just the flags, for cmp and test).
pub(super) fn alu_rm(a: &mut Asm, o: u8, rm: Ea, src: AluSrc, test: bool, slow: DynamicLabel) {
    read_rm(a, rm, 1, slow);
    match src {
        AluSrc::Reg(r) => dynasm!(a ; .arch aarch64 ; mov w4, W(g(r))),
        AluSrc::Imm(v) => mov32(a, 4, v),
    }
    let borrow = alu(a, o).unwrap();
    flags(a, borrow, false);
    if o != 7 && !test {
        match rm {
            Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov W(g(r)), w2),
            Ea::Mem { .. } => store32(a, 2, slow),
        }
    }
    dynasm!(a ; .arch aarch64 ; mov w27, w3);
}
