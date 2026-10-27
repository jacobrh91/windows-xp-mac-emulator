//! The native translation of each instruction form.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};

use super::*;

/// Emit the native form of `op`. Returns false (emitting nothing) if there is none.
/// `next` is the following instruction's address. Control transfers set eip (in w0) and
/// jump to `exit`.
pub(super) fn native(a: &mut Asm, op: &BlockOp, slow: DynamicLabel, exit: DynamicLabel) -> bool {
    match op.op {
        Dop::MovRRm { reg, rm, bits: 32 } => {
            read_rm(a, rm, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov W(g(reg)), w1);
        }
        Dop::MovRmR { rm, reg, bits: 32 } => match rm {
            Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov W(g(r)), W(g(reg))),
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            } => {
                ea(a, base, index, scale, disp);
                store32(a, g(reg), slow);
            }
        },
        Dop::MovRmI { rm, imm } => match rm {
            Ea::Reg(r) => mov32(a, g(r), imm),
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            } => {
                ea(a, base, index, scale, disp);
                mov32(a, 1, imm);
                store32(a, 1, slow);
            }
        },
        Dop::Lea {
            reg,
            ea:
                Ea::Mem {
                    base,
                    index,
                    scale,
                    disp,
                },
        } => {
            ea(a, base, index, scale, disp);
            dynasm!(a ; .arch aarch64 ; mov W(g(reg)), w0);
        }
        Dop::AluRRm {
            op: o,
            reg,
            rm,
            bits: 32,
        } => {
            read_rm(a, rm, 4, slow);
            dynasm!(a ; .arch aarch64 ; mov w1, W(g(reg)));
            let borrow = alu(a, o).unwrap();
            flags(a, borrow, false);
            if o != 7 {
                dynasm!(a ; .arch aarch64 ; mov W(g(reg)), w2);
            }
            dynasm!(a ; .arch aarch64 ; mov w27, w3);
        }
        Dop::AluRmR {
            op: o,
            rm,
            reg,
            bits: 32,
        } => alu_rm(a, o, rm, AluSrc::Reg(reg), false, slow),
        Dop::Test { rm, reg, bits: 32 } => alu_rm(a, 4, rm, AluSrc::Reg(reg), true, slow),
        Dop::AluRmI {
            op: o,
            rm,
            imm,
            bits: 32,
        } => alu_rm(a, o, rm, AluSrc::Imm(imm), false, slow),
        Dop::TestI { rm, imm, bits: 32 } => alu_rm(a, 4, rm, AluSrc::Imm(imm), true, slow),
        Dop::IncDec { rm, dec } => {
            read_rm(a, rm, 1, slow);
            dynasm!(a ; .arch aarch64 ; movz w4, #1);
            if dec {
                dynasm!(a ; .arch aarch64 ; subs w2, w1, w4);
            } else {
                dynasm!(a ; .arch aarch64 ; adds w2, w1, w4);
            }
            flags(a, dec, true);
            match rm {
                Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov W(g(r)), w2),
                Ea::Mem { .. } => store32(a, 2, slow),
            }
            dynasm!(a ; .arch aarch64 ; mov w27, w3);
        }
        Dop::Push(src) => {
            read_rm(a, src, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(ESP)) ; sub w0, w0, #4);
            store32(a, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov W(g(ESP)), w0);
        }
        Dop::PushI(v) => {
            mov32(a, 1, v);
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(ESP)) ; sub w0, w0, #4);
            store32(a, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov W(g(ESP)), w0);
        }
        Dop::Pop(r) => {
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(ESP)));
            load(a, 4, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov w2, W(g(ESP)) ; add w2, w2, #4 ; mov W(g(ESP)), w2 ; mov W(g(r)), w1);
        }
        Dop::Leave => {
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(EBP)));
            load(a, 4, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov w2, W(g(EBP)) ; add w2, w2, #4 ; mov W(g(ESP)), w2 ; mov W(g(EBP)), w1);
        }
        Dop::Movx {
            reg,
            rm,
            bits,
            signed,
        } => {
            match rm {
                Ea::Reg(r) => {
                    if bits == 16 {
                        dynasm!(a ; .arch aarch64 ; mov w1, W(g(r)));
                    } else if r < 4 {
                        dynasm!(a ; .arch aarch64 ; and w1, W(g(r)), #0xFF);
                    } else {
                        dynasm!(a ; .arch aarch64 ; ubfx w1, W(g(r - 4)), #8, #8);
                    }
                }
                Ea::Mem {
                    base,
                    index,
                    scale,
                    disp,
                } => {
                    ea(a, base, index, scale, disp);
                    load(a, bits as u32 / 8, 1, slow);
                }
            }
            match (bits, signed) {
                (8, false) => dynasm!(a ; .arch aarch64 ; uxtb W(g(reg)), w1),
                (8, true) => dynasm!(a ; .arch aarch64 ; sxtb W(g(reg)), w1),
                (_, false) => dynasm!(a ; .arch aarch64 ; uxth W(g(reg)), w1),
                (_, true) => dynasm!(a ; .arch aarch64 ; sxth W(g(reg)), w1),
            }
        }
        Dop::Nop => {}
        Dop::Fpu { opc, reg, rm } => return fpu(a, opc, reg, rm, slow),
        Dop::MovRRm { reg, rm, bits: 16 } => {
            match rm {
                Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov w1, W(g(r))),
                Ea::Mem {
                    base,
                    index,
                    scale,
                    disp,
                } => {
                    ea(a, base, index, scale, disp);
                    load(a, 2, 1, slow);
                }
            }
            dynasm!(a ; .arch aarch64 ; bfi W(g(reg)), w1, #0, #16);
        }
        Dop::MovRmR { rm, reg, bits: 16 } => match rm {
            Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; bfi W(g(r)), W(g(reg)), #0, #16),
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            } => {
                ea(a, base, index, scale, disp);
                store16(a, g(reg), slow);
            }
        },
        Dop::MovRmI8 { rm, imm } => {
            if let Ea::Mem {
                base,
                index,
                scale,
                disp,
            } = rm
            {
                ea(a, base, index, scale, disp);
            }
            mov32(a, 1, imm as u32);
            write8(a, rm, 1, slow);
        }
        Dop::Setcc { cc, rm } => {
            cond(a, cc);
            dynasm!(a ; .arch aarch64 ; mov w1, w0);
            if let Ea::Mem {
                base,
                index,
                scale,
                disp,
            } = rm
            {
                ea(a, base, index, scale, disp);
            }
            write8(a, rm, 1, slow);
        }
        // cdq: edx = eax >> 31 (arithmetic)
        Dop::Cdq => dynasm!(a ; .arch aarch64 ; asr W(g(2)), W(g(0)), #31),
        // movsb / movsd: [edi] = [esi], both stepping by the size (backwards when DF is set)
        Dop::Movs { bits } => {
            let n = bits as u32 / 8;
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(6)));
            load(a, n, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(7)));
            if n == 4 {
                store32(a, 1, slow);
            } else {
                let at = Ea::Mem {
                    base: 8,
                    index: 8,
                    scale: 0,
                    disp: 0,
                };
                write8(a, at, 1, slow);
            }
            dynasm!(a ; .arch aarch64
                ; movz w2, #n
                ; neg w3, w2
                ; tst w27, #0x400
                ; csel w2, w3, w2, ne
                ; add W(g(6)), W(g(6)), w2
                ; add W(g(7)), W(g(7)), w2
            );
        }
        Dop::Unary { neg, rm } => {
            if neg {
                // 0 - rm, as sub computes it
                read_rm(a, rm, 4, slow);
                dynasm!(a ; .arch aarch64 ; movz w1, #0);
                let borrow = alu(a, 5).unwrap();
                flags(a, borrow, false);
            } else {
                read_rm(a, rm, 1, slow);
                dynasm!(a ; .arch aarch64 ; mvn w2, w1);
            }
            match rm {
                Ea::Reg(r) => dynasm!(a ; .arch aarch64 ; mov W(g(r)), w2),
                Ea::Mem { .. } => store32(a, 2, slow),
            }
            if neg {
                dynasm!(a ; .arch aarch64 ; mov w27, w3);
            }
        }
        Dop::MovRRm { reg, rm, bits: 8 } => {
            read8(a, rm, 1, slow);
            write8(a, Ea::Reg(reg), 1, slow);
        }
        Dop::MovRmR { rm, reg, bits: 8 } => {
            if let Ea::Mem {
                base,
                index,
                scale,
                disp,
            } = rm
            {
                ea(a, base, index, scale, disp);
            }
            read8(a, Ea::Reg(reg), 1, slow);
            write8(a, rm, 1, slow);
        }
        Dop::AluRmR {
            op: o,
            rm,
            reg,
            bits: 8,
        } if o != 2 && o != 3 => alu8(a, o, rm, AluSrc::Reg(reg), false, slow),
        Dop::AluRmI {
            op: o,
            rm,
            imm,
            bits: 8,
        } if o != 2 && o != 3 => alu8(a, o, rm, AluSrc::Imm(imm), false, slow),
        Dop::Test { rm, reg, bits: 8 } => alu8(a, 4, rm, AluSrc::Reg(reg), true, slow),
        Dop::TestI { rm, imm, bits: 8 } => alu8(a, 4, rm, AluSrc::Imm(imm), true, slow),
        Dop::AluRRm {
            op: o,
            reg,
            rm,
            bits: 8,
        } if o != 2 && o != 3 => alu8_reg(a, o, reg, rm, slow),
        Dop::Shift {
            op: o @ (4..=7),
            rm,
            count,
            by_cl: false,
        } if count & 31 != 0 => shift_imm(a, o, rm, count as u32 & 31, slow),
        Dop::Shift {
            op: o @ (4..=7),
            rm,
            by_cl: true,
            ..
        } => shift_cl(a, o, rm, slow),
        Dop::Imul { reg, rm } => {
            read_rm(a, rm, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov w4, W(g(reg)));
            imul(a, reg);
        }
        Dop::ImulI { reg, rm, imm } => {
            read_rm(a, rm, 1, slow);
            mov32(a, 4, imm);
            imul(a, reg);
        }
        Dop::CallInd(rm) => {
            read_rm(a, rm, 1, slow);
            mov32(a, 2, op.next);
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(ESP)) ; sub w0, w0, #4);
            store32(a, 2, slow);
            dynasm!(a ; .arch aarch64 ; mov W(g(ESP)), w0 ; mov w0, w1 ; b =>exit);
        }
        Dop::JmpInd(rm) => {
            read_rm(a, rm, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov w0, w1 ; b =>exit);
        }
        // ---- control transfers: eip into w0, then leave the block ----
        Dop::Jcc { cc, target } => {
            cond(a, cc);
            mov32(a, 1, target);
            mov32(a, 2, op.next);
            dynasm!(a ; .arch aarch64 ; cmp w0, #0 ; csel w0, w1, w2, ne ; b =>exit);
        }
        Dop::Jmp(t) => {
            mov32(a, 0, t);
            dynasm!(a ; .arch aarch64 ; b =>exit);
        }
        Dop::Call(t) => {
            mov32(a, 1, op.next);
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(ESP)) ; sub w0, w0, #4);
            store32(a, 1, slow);
            dynasm!(a ; .arch aarch64 ; mov W(g(ESP)), w0);
            mov32(a, 0, t);
            dynasm!(a ; .arch aarch64 ; b =>exit);
        }
        Dop::Ret(n) => {
            let n = n as u32 + 4;
            dynasm!(a ; .arch aarch64 ; mov w0, W(g(ESP)));
            load(a, 4, 1, slow);
            mov32(a, 2, n);
            dynasm!(a ; .arch aarch64 ; add W(g(ESP)), W(g(ESP)), w2 ; mov w0, w1 ; b =>exit);
        }
        _ => return false,
    }
    true
}
