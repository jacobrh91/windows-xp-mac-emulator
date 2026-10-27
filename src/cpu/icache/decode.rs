//! The instruction forms the cache handles (`Dop`) and the decoder that makes them.

use super::super::EAX;

/// "No register" in an effective address.
pub(super) const NONE: u8 = 8;

/// An operand: a register or a memory address computed from registers and a displacement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Ea {
    Reg(u8),
    Mem {
        base: u8,
        index: u8,
        scale: u8,
        disp: u32,
    },
}

/// A decoded instruction. Sizes are 32 bits unless a `bits` field says otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Dop {
    /// Not handled here: run the full interpreter. In a block it is always the last
    /// instruction (its length is not known here).
    Unsupported,
    /// `rm = rm op reg` (op as in `alu`: 0 add .. 7 cmp), 8 or 32 bits.
    AluRmR {
        op: u8,
        rm: Ea,
        reg: u8,
        bits: u8,
    },
    /// `reg = reg op rm`
    AluRRm {
        op: u8,
        reg: u8,
        rm: Ea,
        bits: u8,
    },
    /// `rm = rm op imm`
    AluRmI {
        op: u8,
        rm: Ea,
        imm: u32,
        bits: u8,
    },
    Test {
        rm: Ea,
        reg: u8,
        bits: u8,
    },
    TestI {
        rm: Ea,
        imm: u32,
        bits: u8,
    },
    /// mov rm, reg / mov reg, rm (8 or 32 bits)
    MovRmR {
        rm: Ea,
        reg: u8,
        bits: u8,
    },
    MovRRm {
        reg: u8,
        rm: Ea,
        bits: u8,
    },
    MovRmI {
        rm: Ea,
        imm: u32,
    },
    Lea {
        reg: u8,
        ea: Ea,
    },
    IncDec {
        rm: Ea,
        dec: bool,
    },
    Push(Ea),
    PushI(u32),
    Pop(u8),
    Jcc {
        cc: u8,
        target: u32,
    },
    Jmp(u32),
    Call(u32),
    Ret(u16),
    CallInd(Ea),
    JmpInd(Ea),
    /// movzx / movsx from 8 or 16 bits
    Movx {
        reg: u8,
        rm: Ea,
        bits: u8,
        signed: bool,
    },
    /// shift group (C1 /n imm8, D1 /n by 1, D3 /n by cl): rol, ror, shl, shr, sar
    Shift {
        op: u8,
        rm: Ea,
        count: u8,
        by_cl: bool,
    },
    Leave,
    /// An x87 instruction (D8..DF) with its ModRM already decoded.
    Fpu {
        opc: u8,
        reg: u8,
        rm: Ea,
    },
    Imul {
        reg: u8,
        rm: Ea,
    },
    ImulI {
        reg: u8,
        rm: Ea,
        imm: u32,
    },
    Nop,
    /// mov r/m8, imm8
    MovRmI8 {
        rm: Ea,
        imm: u8,
    },
    /// setcc r/m8
    Setcc {
        cc: u8,
        rm: Ea,
    },
    /// cdq: edx = sign of eax
    Cdq,
    /// movsb / movsd without a rep prefix
    Movs {
        bits: u8,
    },
    /// not / neg r/m32
    Unary {
        neg: bool,
        rm: Ea,
    },
}

/// Does this instruction end a block (it may change where execution goes)?
pub(super) fn ends_block(op: &Dop) -> bool {
    matches!(
        op,
        Dop::Unsupported
            | Dop::Jcc { .. }
            | Dop::Jmp(_)
            | Dop::Call(_)
            | Dop::Ret(_)
            | Dop::CallInd(_)
            | Dop::JmpInd(_)
    )
}

/// Instruction bytes being decoded.
pub(super) struct Bytes<'a> {
    b: &'a [u8],
    p: usize,
}

impl Bytes<'_> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }

    fn u32(&mut self) -> Option<u32> {
        let v = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
    }

    /// ModRM (and SIB/displacement): (reg field, operand).
    fn modrm(&mut self) -> Option<(u8, Ea)> {
        let b = self.u8()?;
        let (md, reg, rm) = (b >> 6, (b >> 3) & 7, b & 7);
        if md == 3 {
            return Some((reg, Ea::Reg(rm)));
        }
        let (mut base, mut index, mut scale, mut disp) = (NONE, NONE, 0, 0u32);
        if rm == 4 {
            let sib = self.u8()?;
            scale = sib >> 6;
            let (idx, bs) = ((sib >> 3) & 7, sib & 7);
            if bs == 5 && md == 0 {
                disp = self.u32()?;
            } else {
                base = bs;
            }
            if idx != 4 {
                index = idx;
            }
        } else if rm == 5 && md == 0 {
            disp = self.u32()?;
        } else {
            base = rm;
        }
        match md {
            1 => disp = disp.wrapping_add(self.u8()? as i8 as u32),
            2 => disp = disp.wrapping_add(self.u32()?),
            _ => {}
        }
        Some((
            reg,
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            },
        ))
    }
}

/// Decode the instruction in `b` (which starts at `start`) if it is one handled here:
/// (length, operation). Prefixed instructions are left to the interpreter.
pub(super) fn decode(b: &[u8], start: u32) -> Option<(u8, Dop)> {
    let mut c = Bytes { b, p: 0 };
    let opc = c.u8()?;
    let op = match opc {
        // the six ALU encodings, 32-bit forms
        0x00..=0x3F if opc & 7 <= 1 => {
            let (reg, rm) = c.modrm()?;
            Dop::AluRmR {
                op: opc >> 3,
                rm,
                reg,
                bits: if opc & 1 == 0 { 8 } else { 32 },
            }
        }
        0x00..=0x3F if opc & 7 <= 3 => {
            let (reg, rm) = c.modrm()?;
            Dop::AluRRm {
                op: opc >> 3,
                reg,
                rm,
                bits: if opc & 1 == 0 { 8 } else { 32 },
            }
        }
        0x00..=0x3F if opc & 7 == 4 => Dop::AluRmI {
            op: opc >> 3,
            rm: Ea::Reg(EAX as u8),
            imm: c.u8()? as u32,
            bits: 8,
        },
        0x00..=0x3F if opc & 7 == 5 => Dop::AluRmI {
            op: opc >> 3,
            rm: Ea::Reg(EAX as u8),
            imm: c.u32()?,
            bits: 32,
        },
        0x40..=0x4F => Dop::IncDec {
            rm: Ea::Reg(opc & 7),
            dec: opc >= 0x48,
        },
        0x50..=0x57 => Dop::Push(Ea::Reg(opc & 7)),
        0x58..=0x5F => Dop::Pop(opc & 7),
        0x68 => Dop::PushI(c.u32()?),
        0x6A => Dop::PushI(c.u8()? as i8 as u32),
        0x69 => {
            let (reg, rm) = c.modrm()?;
            Dop::ImulI {
                reg,
                rm,
                imm: c.u32()?,
            }
        }
        0x6B => {
            let (reg, rm) = c.modrm()?;
            Dop::ImulI {
                reg,
                rm,
                imm: c.u8()? as i8 as u32,
            }
        }
        0x70..=0x7F => {
            let rel = c.u8()? as i8 as u32;
            Dop::Jcc {
                cc: opc & 15,
                target: start.wrapping_add(c.p as u32).wrapping_add(rel),
            }
        }
        0x80 => {
            let (op, rm) = c.modrm()?;
            Dop::AluRmI {
                op,
                rm,
                imm: c.u8()? as u32,
                bits: 8,
            }
        }
        0x81 => {
            let (op, rm) = c.modrm()?;
            Dop::AluRmI {
                op,
                rm,
                imm: c.u32()?,
                bits: 32,
            }
        }
        0x83 => {
            let (op, rm) = c.modrm()?;
            Dop::AluRmI {
                op,
                rm,
                imm: c.u8()? as i8 as u32,
                bits: 32,
            }
        }
        0x84 | 0x85 => {
            let (reg, rm) = c.modrm()?;
            Dop::Test {
                rm,
                reg,
                bits: if opc == 0x84 { 8 } else { 32 },
            }
        }
        0x88 | 0x89 => {
            let (reg, rm) = c.modrm()?;
            Dop::MovRmR {
                rm,
                reg,
                bits: if opc == 0x88 { 8 } else { 32 },
            }
        }
        0x8A | 0x8B => {
            let (reg, rm) = c.modrm()?;
            Dop::MovRRm {
                reg,
                rm,
                bits: if opc == 0x8A { 8 } else { 32 },
            }
        }
        // operand-size prefix: the 16-bit forms of mov, test and the ALU operations
        0x66 => {
            let o = c.u8()?;
            match o {
                0x00..=0x3F if o & 7 == 1 => {
                    let (reg, rm) = c.modrm()?;
                    Dop::AluRmR {
                        op: o >> 3,
                        rm,
                        reg,
                        bits: 16,
                    }
                }
                0x00..=0x3F if o & 7 == 3 => {
                    let (reg, rm) = c.modrm()?;
                    Dop::AluRRm {
                        op: o >> 3,
                        reg,
                        rm,
                        bits: 16,
                    }
                }
                0x81 | 0x83 => {
                    let (op, rm) = c.modrm()?;
                    let imm = if o == 0x81 {
                        u16::from_le_bytes([c.u8()?, c.u8()?]) as u32
                    } else {
                        c.u8()? as i8 as i16 as u16 as u32
                    };
                    Dop::AluRmI {
                        op,
                        rm,
                        imm,
                        bits: 16,
                    }
                }
                0x85 => {
                    let (reg, rm) = c.modrm()?;
                    Dop::Test { rm, reg, bits: 16 }
                }
                0x89 => {
                    let (reg, rm) = c.modrm()?;
                    Dop::MovRmR { rm, reg, bits: 16 }
                }
                0x8B => {
                    let (reg, rm) = c.modrm()?;
                    Dop::MovRRm { reg, rm, bits: 16 }
                }
                _ => return None,
            }
        }
        0x99 => Dop::Cdq,
        // mov eax, [moffs] / mov [moffs], eax
        0xA1 | 0xA3 => {
            let rm = Ea::Mem {
                base: NONE,
                index: NONE,
                scale: 0,
                disp: c.u32()?,
            };
            if opc == 0xA1 {
                Dop::MovRRm {
                    reg: EAX as u8,
                    rm,
                    bits: 32,
                }
            } else {
                Dop::MovRmR {
                    rm,
                    reg: EAX as u8,
                    bits: 32,
                }
            }
        }
        0xA4 => Dop::Movs { bits: 8 },
        0xA5 => Dop::Movs { bits: 32 },
        0xB0..=0xB7 => Dop::MovRmI8 {
            rm: Ea::Reg(opc & 7),
            imm: c.u8()?,
        },
        0xC6 => match c.modrm()? {
            (0, rm) => Dop::MovRmI8 { rm, imm: c.u8()? },
            _ => return None,
        },
        0xF6 => match c.modrm()? {
            (0, rm) => Dop::TestI {
                rm,
                imm: c.u8()? as u32,
                bits: 8,
            },
            _ => return None,
        },
        0xF7 => match c.modrm()? {
            (0, rm) => Dop::TestI {
                rm,
                imm: c.u32()?,
                bits: 32,
            },
            (2, rm) => Dop::Unary { neg: false, rm },
            (3, rm) => Dop::Unary { neg: true, rm },
            _ => return None,
        },
        0x8D => match c.modrm()? {
            (reg, ea @ Ea::Mem { .. }) => Dop::Lea { reg, ea },
            _ => return None,
        },
        0x90 => Dop::Nop,
        0xA8 => Dop::TestI {
            rm: Ea::Reg(EAX as u8),
            imm: c.u8()? as u32,
            bits: 8,
        },
        0xA9 => Dop::TestI {
            rm: Ea::Reg(EAX as u8),
            imm: c.u32()?,
            bits: 32,
        },
        0xB8..=0xBF => Dop::MovRmI {
            rm: Ea::Reg(opc & 7),
            imm: c.u32()?,
        },
        0xC1 | 0xD1 | 0xD3 => {
            let (op, rm) = c.modrm()?;
            if !matches!(op, 0 | 1 | 4..=7) {
                return None; // rcl/rcr
            }
            let count = if opc == 0xC1 { c.u8()? } else { 1 };
            Dop::Shift {
                op,
                rm,
                count,
                by_cl: opc == 0xD3,
            }
        }
        0xC9 => Dop::Leave,
        0xD8..=0xDF => {
            let (reg, rm) = c.modrm()?;
            Dop::Fpu { opc, reg, rm }
        }
        0xC2 => Dop::Ret(u16::from_le_bytes([c.u8()?, c.u8()?])),
        0xC3 => Dop::Ret(0),
        0xC7 => {
            let (_, rm) = c.modrm()?;
            Dop::MovRmI { rm, imm: c.u32()? }
        }
        0xE8 => {
            let rel = c.u32()?;
            Dop::Call(start.wrapping_add(c.p as u32).wrapping_add(rel))
        }
        0xE9 => {
            let rel = c.u32()?;
            Dop::Jmp(start.wrapping_add(c.p as u32).wrapping_add(rel))
        }
        0xEB => {
            let rel = c.u8()? as i8 as u32;
            Dop::Jmp(start.wrapping_add(c.p as u32).wrapping_add(rel))
        }
        0xFF => match c.modrm()? {
            (0, rm) => Dop::IncDec { rm, dec: false },
            (1, rm) => Dop::IncDec { rm, dec: true },
            (2, rm) => Dop::CallInd(rm),
            (4, rm) => Dop::JmpInd(rm),
            (6, rm) => Dop::Push(rm),
            _ => return None,
        },
        0x0F => match c.u8()? {
            0x90..=0x9F => {
                let cc = b[1] & 15;
                let (_, rm) = c.modrm()?;
                Dop::Setcc { cc, rm }
            }
            0x80..=0x8F => {
                let cc = b[1] & 15;
                let rel = c.u32()?;
                Dop::Jcc {
                    cc,
                    target: start.wrapping_add(c.p as u32).wrapping_add(rel),
                }
            }
            op2 @ (0xB6 | 0xB7 | 0xBE | 0xBF) => {
                let (reg, rm) = c.modrm()?;
                Dop::Movx {
                    reg,
                    rm,
                    bits: if op2 & 1 == 0 { 8 } else { 16 },
                    signed: op2 >= 0xBE,
                }
            }
            0xAF => {
                let (reg, rm) = c.modrm()?;
                Dop::Imul { reg, rm }
            }
            _ => return None,
        },
        _ => return None,
    };
    Some((c.p as u8, op))
}
