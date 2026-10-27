//! Executing decoded instructions, with the interpreter's own building blocks.

use super::super::{CF, DF, EAX, EBP, ECX, EDI, EDX, ESI, ESP, Emu, Op, Res, sext};
use super::decode::{Dop, Ea, NONE};

impl Emu {
    #[inline(always)]
    pub(super) fn ea_addr(&self, base: u8, index: u8, scale: u8, disp: u32) -> u32 {
        let mut a = disp;
        if base != NONE {
            a = a.wrapping_add(self.regs[base as usize]);
        }
        if index != NONE {
            a = a.wrapping_add(self.regs[index as usize] << scale);
        }
        a
    }

    #[inline(always)]
    pub(super) fn ea_op(&self, ea: Ea) -> Op {
        match ea {
            Ea::Reg(r) => Op::Reg(r as usize),
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            } => Op::Mem(self.ea_addr(base, index, scale, disp)),
        }
    }

    /// Execute a decoded instruction (at `start`; `next` is the address after it), exactly as
    /// `exec` would.
    #[inline(always)]
    pub(crate) fn exec_dop(&mut self, op: Dop, start: u32, next: u32) -> Res<()> {
        self.eip = next;
        match op {
            Dop::AluRmR { op, rm, reg, bits } => {
                let (rm, bits) = (self.ea_op(rm), bits as u32);
                let a = self.read_op(rm, bits)?;
                let r = self.alu(op as usize, a, self.get_reg(reg as usize, bits), bits);
                if op != 7 {
                    self.write_op(rm, bits, r)?;
                }
            }
            Dop::AluRRm { op, reg, rm, bits } => {
                let bits = bits as u32;
                let b = self.read_op(self.ea_op(rm), bits)?;
                let r = self.alu(op as usize, self.get_reg(reg as usize, bits), b, bits);
                if op != 7 {
                    self.set_reg(reg as usize, bits, r);
                }
            }
            Dop::AluRmI { op, rm, imm, bits } => {
                let (rm, bits) = (self.ea_op(rm), bits as u32);
                let a = self.read_op(rm, bits)?;
                let r = self.alu(op as usize, a, imm, bits);
                if op != 7 {
                    self.write_op(rm, bits, r)?;
                }
            }
            Dop::Test { rm, reg, bits } => {
                let bits = bits as u32;
                let a = self.read_op(self.ea_op(rm), bits)?;
                self.alu(4, a, self.get_reg(reg as usize, bits), bits);
            }
            Dop::TestI { rm, imm, bits } => {
                let bits = bits as u32;
                let a = self.read_op(self.ea_op(rm), bits)?;
                self.alu(4, a, imm, bits);
            }
            Dop::MovRmR { rm, reg, bits } => {
                let v = self.get_reg(reg as usize, bits as u32);
                self.write_op(self.ea_op(rm), bits as u32, v)?;
            }
            Dop::MovRRm { reg, rm, bits } => {
                let v = self.read_op(self.ea_op(rm), bits as u32)?;
                self.set_reg(reg as usize, bits as u32, v);
            }
            Dop::MovRmI { rm, imm } => self.write_op(self.ea_op(rm), 32, imm)?,
            Dop::Lea { reg, ea } => {
                if let Op::Mem(a) = self.ea_op(ea) {
                    self.regs[reg as usize] = a;
                }
            }
            Dop::IncDec { rm, dec } => {
                let rm = self.ea_op(rm);
                let a = self.read_op(rm, 32)?;
                let cf = self.flag(CF);
                let r = self.alu(if dec { 5 } else { 0 }, a, 1, 32);
                self.set_flag(CF, cf);
                self.write_op(rm, 32, r)?;
            }
            Dop::Push(src) => {
                let v = self.read_op(self.ea_op(src), 32)?;
                self.push(v)?;
            }
            Dop::PushI(v) => self.push(v)?,
            Dop::Pop(r) => {
                let v = self.pop()?;
                self.regs[r as usize] = v;
            }
            Dop::Jcc { cc, target } => {
                if self.cond(cc) {
                    self.eip = target;
                }
            }
            Dop::Jmp(t) => self.eip = t,
            Dop::Call(t) => {
                self.push(next)?;
                self.eip = t;
            }
            Dop::Ret(n) => {
                self.eip = self.pop()?;
                self.regs[ESP] = self.regs[ESP].wrapping_add(n as u32);
            }
            Dop::CallInd(rm) => {
                let t = self.read_op(self.ea_op(rm), 32)?;
                self.push(next)?;
                self.eip = t;
            }
            Dop::JmpInd(rm) => self.eip = self.read_op(self.ea_op(rm), 32)?,
            Dop::Movx {
                reg,
                rm,
                bits,
                signed,
            } => {
                let v = self.read_op(self.ea_op(rm), bits as u32)?;
                self.regs[reg as usize] = if signed {
                    sext(v, bits as u32) as u32
                } else {
                    v
                };
            }
            Dop::Shift {
                op,
                rm,
                count,
                by_cl,
            } => {
                let rm = self.ea_op(rm);
                let count = if by_cl {
                    self.regs[ECX] & 0xff
                } else {
                    count as u32
                };
                let v = self.read_op(rm, 32)?;
                let r = self.shift(op as usize, v, count, 32)?;
                self.write_op(rm, 32, r)?;
            }
            Dop::Leave => {
                self.regs[ESP] = self.regs[EBP];
                self.regs[EBP] = self.pop()?;
            }
            Dop::Fpu { opc, reg, rm } => {
                let group = (opc - 0xD8) as usize;
                match self.ea_op(rm) {
                    Op::Mem(addr) => self.fpu_mem(group, reg as usize, addr, start)?,
                    Op::Reg(i) => self.fpu_reg(group, reg as usize, i, start)?,
                }
            }
            Dop::Imul { reg, rm } => {
                let b = self.read_op(self.ea_op(rm), 32)?;
                let r = self.imul2(self.regs[reg as usize], b, 32);
                self.regs[reg as usize] = r;
            }
            Dop::ImulI { reg, rm, imm } => {
                let a = self.read_op(self.ea_op(rm), 32)?;
                let r = self.imul2(a, imm, 32);
                self.regs[reg as usize] = r;
            }
            Dop::Nop => {}
            Dop::MovRmI8 { rm, imm } => self.write_op(self.ea_op(rm), 8, imm as u32)?,
            Dop::Setcc { cc, rm } => {
                let v = self.cond(cc) as u32;
                self.write_op(self.ea_op(rm), 8, v)?;
            }
            Dop::Cdq => self.regs[EDX] = ((self.regs[EAX] as i32) >> 31) as u32,
            Dop::Movs { bits } => {
                let bits = bits as u32;
                let v = self.read_op(Op::Mem(self.regs[ESI]), bits)?;
                self.write_op(Op::Mem(self.regs[EDI]), bits, v)?;
                let n = bits / 8;
                let d = if self.eflags & DF != 0 {
                    n.wrapping_neg()
                } else {
                    n
                };
                self.regs[ESI] = self.regs[ESI].wrapping_add(d);
                self.regs[EDI] = self.regs[EDI].wrapping_add(d);
            }
            Dop::Unary { neg, rm } => {
                let op = self.ea_op(rm);
                let a = self.read_op(op, 32)?;
                // neg is 0 - a, flags and all (CF: a was not zero)
                let r = if neg { self.alu(5, 0, a, 32) } else { !a };
                self.write_op(op, 32, r)?;
            }
            Dop::Unsupported => {
                self.eip = start;
                return self.step_decoded(start);
            }
        }
        Ok(())
    }
}
