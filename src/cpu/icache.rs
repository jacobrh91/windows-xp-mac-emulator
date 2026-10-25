//! Decoded-instruction cache.
//!
//! The interpreter normally decodes every instruction each time it runs it: prefixes, opcode,
//! ModRM, SIB, displacement and immediate, one fetched byte at a time. Here the common integer
//! instructions are decoded once into a `Dop` (operands resolved, branch targets made absolute)
//! and kept in a direct-mapped table indexed by address, so running them again costs one tag
//! compare. Everything else (x87, MMX, SSE, prefixed and rare instructions) is remembered as
//! `Unsupported` and goes through the full interpreter as before.
//!
//! Execution uses the interpreter's own building blocks (`alu`, `shift`, `push`, `cond`, ...),
//! so the results, flags and faults are the same either way. Writing to a page that holds cached
//! instructions (self-modifying code, a DLL loaded over a freed one, a hook being patched in)
//! marks the cache stale and it is emptied before the next instruction.
//! Straight runs of decoded instructions are also kept as *blocks* (up to the next branch,
//! call, return or instruction not handled here), which the run loop executes back to back
//! without its per-instruction dispatch.
//!
//! `XP_NO_ICACHE=1` turns it off.

use std::rc::Rc;

use super::{sext, Emu, Op, Res, Stop, CF, EAX, EBP, ECX, ESP};
use crate::win32::{HLE_BASE, HLE_END};

/// Entries in the table (a power of two).
const SLOTS: usize = 1 << 16;
/// Tag of an empty slot (no instruction lives at the very top of the address space).
const EMPTY: u32 = u32::MAX;
/// "No register" in an effective address.
const NONE: u8 = 8;

/// An operand: a register or a memory address computed from registers and a displacement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Ea {
    Reg(u8),
    Mem { base: u8, index: u8, scale: u8, disp: u32 },
}

/// A decoded instruction. Sizes are 32 bits unless a `bits` field says otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Dop {
    /// Not handled here: run the full interpreter.
    Unsupported,
    /// `rm = rm op reg` (op as in `alu`: 0 add .. 7 cmp), 8 or 32 bits.
    AluRmR { op: u8, rm: Ea, reg: u8, bits: u8 },
    /// `reg = reg op rm`
    AluRRm { op: u8, reg: u8, rm: Ea, bits: u8 },
    /// `rm = rm op imm`
    AluRmI { op: u8, rm: Ea, imm: u32, bits: u8 },
    Test { rm: Ea, reg: u8, bits: u8 },
    TestI { rm: Ea, imm: u32, bits: u8 },
    /// mov rm, reg / mov reg, rm (8 or 32 bits)
    MovRmR { rm: Ea, reg: u8, bits: u8 },
    MovRRm { reg: u8, rm: Ea, bits: u8 },
    MovRmI { rm: Ea, imm: u32 },
    Lea { reg: u8, ea: Ea },
    IncDec { rm: Ea, dec: bool },
    Push(Ea),
    PushI(u32),
    Pop(u8),
    Jcc { cc: u8, target: u32 },
    Jmp(u32),
    Call(u32),
    Ret(u16),
    CallInd(Ea),
    JmpInd(Ea),
    /// movzx / movsx from 8 or 16 bits
    Movx { reg: u8, rm: Ea, bits: u8, signed: bool },
    /// shift group (C1 /n imm8, D1 /n by 1, D3 /n by cl): rol, ror, shl, shr, sar
    Shift { op: u8, rm: Ea, count: u8, by_cl: bool },
    Leave,
    /// An x87 instruction (D8..DF) with its ModRM already decoded.
    Fpu { opc: u8, reg: u8, rm: Ea },
    Imul { reg: u8, rm: Ea },
    ImulI { reg: u8, rm: Ea, imm: u32 },
    Nop,
}

#[derive(Clone, Copy)]
pub(crate) struct Entry {
    eip: u32,
    len: u8,
    op: Dop,
}

/// Entries in the block table (a power of two).
const BLOCK_SLOTS: usize = 1 << 19;
/// Longest block, in instructions.
const MAX_BLOCK: usize = 32;

/// One instruction of a block: the operation, where it starts and where the next one starts.
#[derive(Clone, Copy)]
pub(crate) struct BlockOp {
    pub(crate) op: Dop,
    pub(crate) start: u32,
    pub(crate) next: u32,
}

/// A run of decoded instructions starting at `start`. Empty when the first one is not
/// handled here (so the lookup is not repeated).
#[derive(Clone)]
pub(crate) struct Block {
    start: u32,
    ops: Rc<[BlockOp]>,
    /// Times run since built, and compiled code (0: none) from JIT generation `gen`.
    hits: u32,
    code: usize,
    jit_gen: u32,
}

pub struct ICache {
    entries: Vec<Entry>,
    blocks: Vec<Block>,
    /// Blocks built and block runs (statistics for XP_FPS).
    pub(crate) built: u64,
    pub(crate) runs: u64,
    pub(crate) flushes: u64,
}

impl ICache {
    pub(crate) fn new() -> ICache {
        let empty: Rc<[BlockOp]> = Rc::new([]);
        ICache {
            entries: vec![Entry { eip: EMPTY, len: 0, op: Dop::Unsupported }; SLOTS],
            blocks: vec![Block { start: EMPTY, ops: empty, hits: 0, code: 0, jit_gen: 0 }; BLOCK_SLOTS],
            built: 0,
            runs: 0,
            flushes: 0,
        }
    }

    fn clear(&mut self) {
        self.flushes += 1;
        for e in self.entries.iter_mut() {
            e.eip = EMPTY;
        }
        for b in self.blocks.iter_mut() {
            b.start = EMPTY;
        }
    }
}

#[inline(always)]
fn slot(eip: u32) -> usize {
    ((eip ^ (eip >> 16)) as usize) & (SLOTS - 1)
}

#[inline(always)]
fn block_slot(eip: u32) -> usize {
    ((eip ^ (eip >> 19)) as usize) & (BLOCK_SLOTS - 1)
}

/// Does this instruction end a block (it may change where execution goes)?
fn ends_block(op: &Dop) -> bool {
    matches!(op, Dop::Jcc { .. } | Dop::Jmp(_) | Dop::Call(_) | Dop::Ret(_) | Dop::CallInd(_) | Dop::JmpInd(_))
}

/// Instruction bytes being decoded.
struct Bytes<'a> {
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
        Some((reg, Ea::Mem { base, index, scale, disp }))
    }
}

/// Decode the instruction in `b` (which starts at `start`) if it is one handled here:
/// (length, operation). Prefixed instructions are left to the interpreter.
fn decode(b: &[u8], start: u32) -> Option<(u8, Dop)> {
    let mut c = Bytes { b, p: 0 };
    let opc = c.u8()?;
    let op = match opc {
        // the six ALU encodings, 32-bit forms
        0x00..=0x3F if opc & 7 <= 1 => {
            let (reg, rm) = c.modrm()?;
            Dop::AluRmR { op: opc >> 3, rm, reg, bits: if opc & 1 == 0 { 8 } else { 32 } }
        }
        0x00..=0x3F if opc & 7 <= 3 => {
            let (reg, rm) = c.modrm()?;
            Dop::AluRRm { op: opc >> 3, reg, rm, bits: if opc & 1 == 0 { 8 } else { 32 } }
        }
        0x00..=0x3F if opc & 7 == 4 => Dop::AluRmI { op: opc >> 3, rm: Ea::Reg(EAX as u8), imm: c.u8()? as u32, bits: 8 },
        0x00..=0x3F if opc & 7 == 5 => Dop::AluRmI { op: opc >> 3, rm: Ea::Reg(EAX as u8), imm: c.u32()?, bits: 32 },
        0x40..=0x4F => Dop::IncDec { rm: Ea::Reg(opc & 7), dec: opc >= 0x48 },
        0x50..=0x57 => Dop::Push(Ea::Reg(opc & 7)),
        0x58..=0x5F => Dop::Pop(opc & 7),
        0x68 => Dop::PushI(c.u32()?),
        0x6A => Dop::PushI(c.u8()? as i8 as u32),
        0x69 => {
            let (reg, rm) = c.modrm()?;
            Dop::ImulI { reg, rm, imm: c.u32()? }
        }
        0x6B => {
            let (reg, rm) = c.modrm()?;
            Dop::ImulI { reg, rm, imm: c.u8()? as i8 as u32 }
        }
        0x70..=0x7F => {
            let rel = c.u8()? as i8 as u32;
            Dop::Jcc { cc: opc & 15, target: start.wrapping_add(c.p as u32).wrapping_add(rel) }
        }
        0x80 => {
            let (op, rm) = c.modrm()?;
            Dop::AluRmI { op, rm, imm: c.u8()? as u32, bits: 8 }
        }
        0x81 => {
            let (op, rm) = c.modrm()?;
            Dop::AluRmI { op, rm, imm: c.u32()?, bits: 32 }
        }
        0x83 => {
            let (op, rm) = c.modrm()?;
            Dop::AluRmI { op, rm, imm: c.u8()? as i8 as u32, bits: 32 }
        }
        0x84 | 0x85 => {
            let (reg, rm) = c.modrm()?;
            Dop::Test { rm, reg, bits: if opc == 0x84 { 8 } else { 32 } }
        }
        0x88 | 0x89 => {
            let (reg, rm) = c.modrm()?;
            Dop::MovRmR { rm, reg, bits: if opc == 0x88 { 8 } else { 32 } }
        }
        0x8A | 0x8B => {
            let (reg, rm) = c.modrm()?;
            Dop::MovRRm { reg, rm, bits: if opc == 0x8A { 8 } else { 32 } }
        }
        0x8D => match c.modrm()? {
            (reg, ea @ Ea::Mem { .. }) => Dop::Lea { reg, ea },
            _ => return None,
        },
        0x90 => Dop::Nop,
        0xA8 => Dop::TestI { rm: Ea::Reg(EAX as u8), imm: c.u8()? as u32, bits: 8 },
        0xA9 => Dop::TestI { rm: Ea::Reg(EAX as u8), imm: c.u32()?, bits: 32 },
        0xB8..=0xBF => Dop::MovRmI { rm: Ea::Reg(opc & 7), imm: c.u32()? },
        0xC1 | 0xD1 | 0xD3 => {
            let (op, rm) = c.modrm()?;
            if !matches!(op, 0 | 1 | 4..=7) {
                return None; // rcl/rcr
            }
            let count = if opc == 0xC1 { c.u8()? } else { 1 };
            Dop::Shift { op, rm, count, by_cl: opc == 0xD3 }
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
            0x80..=0x8F => {
                let cc = b[1] & 15;
                let rel = c.u32()?;
                Dop::Jcc { cc, target: start.wrapping_add(c.p as u32).wrapping_add(rel) }
            }
            op2 @ (0xB6 | 0xB7 | 0xBE | 0xBF) => {
                let (reg, rm) = c.modrm()?;
                Dop::Movx { reg, rm, bits: if op2 & 1 == 0 { 8 } else { 16 }, signed: op2 >= 0xBE }
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

impl Emu {
    #[inline(always)]
    fn ea_addr(&self, base: u8, index: u8, scale: u8, disp: u32) -> u32 {
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
    fn ea_op(&self, ea: Ea) -> Op {
        match ea {
            Ea::Reg(r) => Op::Reg(r as usize),
            Ea::Mem { base, index, scale, disp } => Op::Mem(self.ea_addr(base, index, scale, disp)),
        }
    }

    /// Run up to `limit` instructions from the block starting at `eip`. None when there is no
    /// block to run here (an API stub, or an instruction the cache does not handle): the caller
    /// takes the ordinary single-step path. Otherwise (instructions executed, outcome); a
    /// fault is handled exactly as `step` would handle it.
    #[inline(always)]
    pub(crate) fn run_block(&mut self, limit: u64) -> Option<(u64, Res<()>)> {
        let start = self.eip;
        if start.wrapping_sub(HLE_BASE) < HLE_END - HLE_BASE || start == self.seh_ret {
            return None;
        }
        if self.mem.code_dirty {
            self.icache.clear();
            self.mem.forget_code_pages();
            self.jit.reset();
        }
        let i = block_slot(start);
        if self.icache.blocks[i].start != start {
            self.build_block(i, start);
        }
        let ops = self.icache.blocks[i].ops.clone();
        if ops.is_empty() {
            return None;
        }
        self.icache.runs += 1;
        let n = ops.len().min(limit as usize);
        if self.jit.enabled && n == ops.len() {
            if let Some(r) = self.run_compiled(i, start) {
                return Some(r);
            }
        }
        for (k, b) in ops[..n].iter().enumerate() {
            self.steps += 1;
            self.recent[(self.steps & 15) as usize] = b.start;
            let saved = (self.regs, self.eflags);
            if let Err(e) = self.exec_dop(b.op, b.start, b.next) {
                let r = match e {
                    // A hardware fault is a Windows exception, as in `step`.
                    stop @ (Stop::Fault(_) | Stop::DivideError(_) | Stop::InvalidOpcode { .. }) => self.raise_hardware(stop, b.start, saved),
                    other => Err(other),
                };
                return Some((k as u64 + 1, r));
            }
            if self.mem.code_dirty {
                // It just wrote to cached code: the rest of this block may be stale.
                return Some((k as u64 + 1, Ok(())));
            }
        }
        Some((n as u64, Ok(())))
    }

    /// Run block `i` as compiled code, compiling it once it is hot. None: not compiled (yet).
    #[inline(always)]
    fn run_compiled(&mut self, i: usize, start: u32) -> Option<(u64, Res<()>)> {
        let current = self.jit.generation;
        let b = &mut self.icache.blocks[i];
        if b.jit_gen != current || b.code == 0 {
            b.hits += 1;
            if b.hits < super::jit::HOT {
                return None;
            }
            b.hits = 0;
            let ops = b.ops.clone();
            let f = match self.jit.compile(&ops) {
                Some(f) => f,
                None => {
                    // The code area is full: start it afresh (later blocks compile again).
                    self.jit.reset();
                    return None;
                }
            };
            let b = &mut self.icache.blocks[i];
            b.code = f as usize;
            b.jit_gen = self.jit.generation;
        }
        let code = self.icache.blocks[i].code;
        (self.jit_pages, self.jit_code_pages) = self.mem.page_table();
        // SAFETY: `code` was produced by `jit.compile` for this block in the current
        // generation; it only touches this emulator through the pointer it is given.
        let f: super::jit::BlockFn = unsafe { std::mem::transmute::<usize, super::jit::BlockFn>(code) };
        let r = unsafe { f(self) };
        self.jit.runs += 1;
        let count = r & 0xFFFF_FFFF;
        self.steps += count;
        self.recent[(self.steps & 15) as usize] = start;
        if r >> 32 != 0 {
            let stop = self.jit_stop.take().unwrap_or(Stop::Halt);
            return Some((count, Err(stop)));
        }
        Some((count, Ok(())))
    }

    #[cold]
    #[inline(never)]
    fn build_block(&mut self, i: usize, start: u32) {
        let mut ops = Vec::new();
        let mut at = start;
        while ops.len() < MAX_BLOCK {
            let mut buf = [0u8; 16];
            let mut n = 0;
            while n < buf.len() {
                match self.mem.read_u8(at.wrapping_add(n as u32)) {
                    Ok(b) => buf[n] = b,
                    Err(_) => break,
                }
                n += 1;
            }
            self.mem.note_code(at, buf.len() as u32);
            let Some((len, op)) = decode(&buf[..n], at) else { break };
            let next = at.wrapping_add(len as u32);
            ops.push(BlockOp { op, start: at, next });
            if ends_block(&op) || next.wrapping_sub(HLE_BASE) < HLE_END - HLE_BASE {
                break;
            }
            at = next;
        }
        self.icache.built += 1;
        self.icache.blocks[i] = Block { start, ops: ops.into(), hits: 0, code: 0, jit_gen: 0 };
    }

    /// Run the instruction at `start` through the cache: decoded once, then reused.
    #[inline(always)]
    pub(crate) fn step_cached(&mut self, start: u32) -> Res<()> {
        if self.mem.code_dirty {
            self.icache.clear();
            self.mem.forget_code_pages();
        }
        let i = slot(start);
        let e = self.icache.entries[i];
        if e.eip == start {
            return match e.op {
                Dop::Unsupported => self.step_decoded(start),
                op => self.exec_dop(op, start, start.wrapping_add(e.len as u32)),
            };
        }
        self.fill_slot(i, start)
    }

    #[cold]
    #[inline(never)]
    fn fill_slot(&mut self, i: usize, start: u32) -> Res<()> {
        let mut buf = [0u8; 16];
        let mut n = 0;
        while n < buf.len() {
            match self.mem.read_u8(start.wrapping_add(n as u32)) {
                Ok(b) => buf[n] = b,
                Err(_) => break,
            }
            n += 1;
        }
        let (len, op) = decode(&buf[..n], start).unwrap_or((0, Dop::Unsupported));
        // Writing to these pages from now on must throw this entry away.
        // (All 16 bytes looked at: an instruction cut short by an unmapped page decodes
        // differently once that page exists.)
        self.mem.note_code(start, buf.len() as u32);
        self.icache.entries[i] = Entry { eip: start, len, op };
        match op {
            Dop::Unsupported => self.step_decoded(start),
            op => self.exec_dop(op, start, start.wrapping_add(len as u32)),
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
            Dop::Movx { reg, rm, bits, signed } => {
                let v = self.read_op(self.ea_op(rm), bits as u32)?;
                self.regs[reg as usize] = if signed { sext(v, bits as u32) as u32 } else { v };
            }
            Dop::Shift { op, rm, count, by_cl } => {
                let rm = self.ea_op(rm);
                let count = if by_cl { self.regs[ECX] & 0xff } else { count as u32 };
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
            Dop::Unsupported => unreachable!("unsupported instructions go to the interpreter"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::{Stop, EBX, ESI};

    #[test]
    fn decodes_operands_and_absolute_targets() {
        // mov eax, [ebx+esi*4+0x10]
        assert_eq!(decode(&[0x8B, 0x44, 0xB3, 0x10], 0), Some((4, Dop::MovRRm { reg: 0, rm: Ea::Mem { base: 3, index: 6, scale: 2, disp: 0x10 }, bits: 32 })));
        // jne -2 at 0x1000: back to itself
        assert_eq!(decode(&[0x75, 0xFE], 0x1000), Some((2, Dop::Jcc { cc: 5, target: 0x1000 })));
        // call +0x100 at 0x2000
        assert_eq!(decode(&[0xE8, 0x00, 0x01, 0, 0], 0x2000), Some((5, Dop::Call(0x2105))));
        // [disp32] with no base
        assert_eq!(decode(&[0x8B, 0x05, 0x78, 0x56, 0x34, 0x12], 0).map(|d| d.1), Some(Dop::MovRRm { reg: 0, rm: Ea::Mem { base: NONE, index: NONE, scale: 0, disp: 0x1234_5678 }, bits: 32 }));
        assert_eq!(decode(&[0x66, 0x89, 0xC0], 0), None, "prefixed: left to the interpreter");
        assert_eq!(decode(&[0xD9, 0xE8], 0), Some((2, Dop::Fpu { opc: 0xD9, reg: 5, rm: Ea::Reg(0) })), "fld1, ModRM decoded");
        assert_eq!(decode(&[0x8B], 0), None, "truncated");
    }

    /// Tiny random generator (no external crates in tests).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn pick<T: Copy>(&mut self, v: &[T]) -> T {
            v[self.next() as usize % v.len()]
        }
    }

    /// A random instruction the cache handles, touching memory only through [ebx+disp8] /
    /// [esi+disp8] (both kept pointing into the data area) and never writing ebx, esi or esp.
    fn random_insn(r: &mut Rng, out: &mut Vec<u8>) {
        let dst_reg = |r: &mut Rng| r.pick(&[0u8, 1, 2, 5, 7]); // eax ecx edx ebp edi
        let modrm_reg_mem = |r: &mut Rng, reg: u8| -> Vec<u8> {
            if r.next() % 2 == 0 {
                vec![0xC0 | (reg << 3) | r.pick(&[0u8, 1, 2, 3, 5, 6, 7])]
            } else {
                let base = r.pick(&[3u8, 6]);
                vec![0x40 | (reg << 3) | base, (r.next() % 0x7C) as u8 & !3]
            }
        };
        // destination is a written operand: register form only for our "written" regs, or memory
        let modrm_dst = |r: &mut Rng, reg: u8| -> Vec<u8> {
            if r.next() % 2 == 0 {
                vec![0xC0 | (reg << 3) | dst_reg(r)]
            } else {
                let base = r.pick(&[3u8, 6]);
                vec![0x40 | (reg << 3) | base, (r.next() % 0x7C) as u8 & !3]
            }
        };
        match r.next() % 15 {
            12 => {
                // 8-bit mov in either direction
                out.push(r.pick(&[0x88u8, 0x8A]));
                let reg = r.pick(&[0u8, 1, 2, 4, 5, 6]); // al cl dl ah ch dh
                out.extend(modrm_dst(r, reg));
            }
            13 => {
                // 8-bit alu with an immediate, or test r/m8, r8
                if r.next() % 2 == 0 {
                    out.push(0x80);
                    let op = (r.next() % 8) as u8;
                    out.extend(modrm_dst(r, op));
                    out.push(r.next() as u8);
                } else {
                    out.push(0x84);
                    let reg = r.pick(&[0u8, 1, 2, 3, 4, 5, 6, 7]);
                    out.extend(modrm_dst(r, reg));
                }
            }
            14 => {
                // al, imm8 forms and imul with an immediate
                if r.next() % 2 == 0 {
                    out.extend([((r.next() % 8) as u8) << 3 | 4, r.next() as u8]);
                } else {
                    out.push(0x6B);
                    let reg = dst_reg(r);
                    out.extend(modrm_reg_mem(r, reg));
                    out.push(r.next() as u8);
                }
            }
            0 => {
                // alu r/m, r (dst may be memory)
                let op = (r.next() % 8) as u8;
                out.push(op << 3 | r.pick(&[0u8, 1]));
                let reg = r.pick(&[0u8, 1, 2, 3, 5, 6, 7]);
                out.extend(modrm_dst(r, reg));
            }
            1 => {
                let op = (r.next() % 8) as u8;
                out.push(op << 3 | r.pick(&[2u8, 3]));
                let reg = dst_reg(r);
                out.extend(modrm_reg_mem(r, reg));
            }
            2 => {
                out.push(r.pick(&[0x81u8, 0x83]));
                let op = (r.next() % 8) as u8;
                let rest = modrm_dst(r, op);
                let imm8 = out[out.len() - 1] == 0x83;
                out.extend(rest);
                if imm8 {
                    out.push(r.next() as u8);
                } else {
                    out.extend(r.next().to_le_bytes());
                }
            }
            3 => {
                out.push(0x8B);
                let reg = dst_reg(r);
                out.extend(modrm_reg_mem(r, reg));
            }
            4 => {
                out.push(0x89);
                let reg = r.pick(&[0u8, 1, 2, 3, 5, 6, 7]);
                out.extend(modrm_dst(r, reg));
            }
            5 => {
                out.push(0x8D);
                let reg = dst_reg(r);
                out.extend([0x44 | (reg << 3), 0x73, r.next() as u8]); // lea reg, [ebx+esi*2+d8]
            }
            6 => out.push(r.pick(&[0x40u8, 0x41, 0x42, 0x45, 0x47, 0x48, 0x49, 0x4A, 0x4D, 0x4F])),
            7 => {
                let opc = r.pick(&[0xC1u8, 0xD1, 0xD3]);
                out.push(opc);
                let op = r.pick(&[0u8, 1, 4, 5, 7]);
                out.extend(modrm_dst(r, op));
                if opc == 0xC1 {
                    out.push((r.next() % 40) as u8);
                }
            }
            8 => {
                out.extend([0x0F, r.pick(&[0xB6u8, 0xB7, 0xBE, 0xBF])]);
                let reg = dst_reg(r);
                out.extend(modrm_reg_mem(r, reg));
            }
            9 => {
                out.extend([0x0F, 0xAF]);
                let reg = dst_reg(r);
                out.extend(modrm_reg_mem(r, reg));
            }
            10 => {
                // push then pop into a written register
                out.push(0x50 | r.pick(&[0u8, 1, 2, 3, 5, 6, 7]));
                out.push(0x58 | dst_reg(r));
            }
            _ => {
                // a conditional skip over a 1-byte inc, so both directions run
                out.extend([0x70 | (r.next() % 16) as u8, 0x01, 0x40 | dst_reg(r)]);
            }
        }
    }

    fn machine(code: &[u8], seed: u64, cached: bool) -> Emu {
        let mut e = Emu::new();
        e.icache_on = cached;
        e.mem.map(0x1000, 0x20000);
        e.mem.write_bytes(0x1000, code).unwrap();
        e.mem.write_bytes(0x1000 + code.len() as u32, &[0xF4]).unwrap(); // hlt
        let mut r = Rng(seed);
        for a in (0x10000..0x10400).step_by(4) {
            e.mem.write(a, 4, r.next()).unwrap();
        }
        for (i, v) in e.regs.iter_mut().enumerate() {
            *v = r.next();
            if i == EBX {
                *v = 0x10000;
            }
            if i == ESI {
                *v = 0x10100;
            }
        }
        e.regs[ESP] = 0x18000;
        e.eflags = 2 | (r.next() & 0x8D5);
        e.eip = 0x1000;
        e
    }

    #[test]
    fn cached_execution_matches_the_interpreter_on_random_code() {
        for seed in 1..=60u64 {
            let mut r = Rng(seed * 7919);
            let mut code = Vec::new();
            for _ in 0..200 {
                random_insn(&mut r, &mut code);
            }
            // Run it twice over (a loop back to the start) so cached entries are reused.
            let mut a = machine(&code, seed, false);
            let mut b = machine(&code, seed, true);
            for _ in 0..2 {
                a.eip = 0x1000;
                b.eip = 0x1000;
                assert_eq!(a.run(10_000), Stop::Halt);
                assert_eq!(b.run(10_000), Stop::Halt);
            }
            assert_eq!(a.regs, b.regs, "registers (seed {seed})");
            assert_eq!(a.eflags, b.eflags, "flags (seed {seed})");
            assert_eq!(a.steps, b.steps, "instruction count (seed {seed})");
            assert_eq!(a.mem.read_bytes(0x10000, 0x400).unwrap(), b.mem.read_bytes(0x10000, 0x400).unwrap(), "memory (seed {seed})");
            assert_eq!(a.mem.read_bytes(0x17F00, 0x100).unwrap(), b.mem.read_bytes(0x17F00, 0x100).unwrap(), "stack (seed {seed})");
        }
    }

    /// The same random code, run often enough for its blocks to be compiled, with the JIT
    /// against the plain interpreter.
    #[test]
    fn compiled_blocks_match_the_interpreter_on_random_code() {
        let mut compiled = 0;
        for seed in 1..=40u64 {
            let mut r = Rng(seed * 104_729);
            let mut code = Vec::new();
            for _ in 0..120 {
                random_insn(&mut r, &mut code);
            }
            let mut a = machine(&code, seed, false);
            a.jit.enabled = false;
            let mut b = machine(&code, seed, true);
            if !b.jit.enabled {
                return; // not an Apple Silicon Mac
            }
            for pass in 0..40 {
                a.eip = 0x1000;
                b.eip = 0x1000;
                assert_eq!(a.run(100_000), Stop::Halt);
                assert_eq!(b.run(100_000), Stop::Halt);
                assert_eq!(a.regs, b.regs, "registers (seed {seed}, pass {pass})");
                assert_eq!(a.eflags, b.eflags, "flags (seed {seed}, pass {pass})");
            }
            assert_eq!(a.steps, b.steps, "instruction count (seed {seed})");
            assert_eq!(a.mem.read_bytes(0x10000, 0x400).unwrap(), b.mem.read_bytes(0x10000, 0x400).unwrap(), "memory (seed {seed})");
            assert_eq!(a.mem.read_bytes(0x17F00, 0x100).unwrap(), b.mem.read_bytes(0x17F00, 0x100).unwrap(), "stack (seed {seed})");
            compiled += b.jit.compiled;
        }
        assert!(compiled > 0, "blocks were actually compiled");
    }

    #[test]
    fn rewriting_code_drops_the_stale_decoding() {
        // mov eax, 1 ; hlt -- run, patch to mov eax, 2, run again
        let mut e = machine(&[0xB8, 1, 0, 0, 0], 1, true);
        assert_eq!(e.run(100), Stop::Halt);
        assert_eq!(e.regs[EAX], 1);
        e.mem.write(0x1001, 1, 2).unwrap();
        e.eip = 0x1000;
        assert_eq!(e.run(100), Stop::Halt);
        assert_eq!(e.regs[EAX], 2, "the new instruction ran, not the cached one");
    }

    #[test]
    fn faults_inside_cached_instructions_are_precise() {
        // mov [0x00000010], eax -- unmapped: the fault leaves eip on the instruction
        let mut e = machine(&[0x89, 0x05, 0x10, 0, 0, 0], 1, true);
        let stop = e.run(100);
        assert!(matches!(stop, Stop::Fault(_)), "{stop:?}");
        assert_eq!(e.eip, 0x1000);
    }
}
