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

use super::{CF, DF, EAX, EBP, ECX, EDI, EDX, ESI, ESP, Emu, Op, Res, Stop, sext};
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
    MovRmI8 { rm: Ea, imm: u8 },
    /// setcc r/m8
    Setcc { cc: u8, rm: Ea },
    /// cdq: edx = sign of eax
    Cdq,
    /// movsb / movsd without a rep prefix
    Movs { bits: u8 },
    /// not / neg r/m32
    Unary { neg: bool, rm: Ea },
}

#[derive(Clone, Copy)]
pub(crate) struct Entry {
    eip: u32,
    len: u8,
    op: Dop,
}

/// Entries in the block table (a power of two), in sets of `WAYS`: a block can sit in any
/// entry of its set, so two hot blocks whose addresses hash alike do not keep evicting each
/// other.
const BLOCK_SLOTS: usize = 1 << 19;
const WAYS: usize = 4;
const SET_BITS: u32 = (BLOCK_SLOTS / WAYS).trailing_zeros();
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
    /// `ICache::clock` when last run: the set's least recently run block is replaced.
    used: u32,
}

pub struct ICache {
    entries: Vec<Entry>,
    blocks: Vec<Block>,
    /// Counts block lookups, for `Block::used`.
    clock: u32,
    /// Blocks built and block runs (statistics for XP_FPS).
    pub(crate) built: u64,
    pub(crate) runs: u64,
    pub(crate) flushes: u64,
}

impl ICache {
    pub(crate) fn new() -> ICache {
        let empty: Rc<[BlockOp]> = Rc::new([]);
        ICache {
            entries: vec![
                Entry {
                    eip: EMPTY,
                    len: 0,
                    op: Dop::Unsupported
                };
                SLOTS
            ],
            blocks: vec![
                Block {
                    start: EMPTY,
                    ops: empty,
                    hits: 0,
                    code: 0,
                    jit_gen: 0,
                    used: 0,
                };
                BLOCK_SLOTS
            ],
            clock: 0,
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
/// XP_COVER=<lo>-<hi> (hex): the address range whose blocks are reported at their first run,
/// and kept out of compiled code.
pub(crate) fn cover_range() -> Option<(u32, u32)> {
    static R: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *R.get_or_init(|| {
        let v = std::env::var("XP_COVER").ok()?;
        let (a, b) = v.split_once('-')?;
        Some((u32::from_str_radix(a.trim(), 16).ok()?, u32::from_str_radix(b.trim(), 16).ok()?))
    })
}

/// XP_TRACE_AT=<hex>,...: addresses whose blocks are logged each time they run (and kept out
/// of compiled code, so that every run passes through here).
pub(crate) fn trace_at(eip: u32) -> bool {
    static AT: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let at = AT.get_or_init(|| {
        std::env::var("XP_TRACE_AT")
            .map(|v| v.split(',').filter_map(|a| u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).ok()).collect())
            .unwrap_or_default()
    });
    !at.is_empty() && at.contains(&eip)
}

/// The first entry of the set a block starting at `eip` belongs to. A multiplicative
/// (Fibonacci) hash: code addresses that differ only in their high bits, as a program's
/// functions do, still spread over the whole table.
fn block_set(eip: u32) -> usize {
    (eip.wrapping_mul(0x9E37_79B1) >> (32 - SET_BITS)) as usize * WAYS
}

/// Does this instruction end a block (it may change where execution goes)?
fn ends_block(op: &Dop) -> bool {
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
fn decode(b: &[u8], start: u32) -> Option<(u8, Dop)> {
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
            Ea::Mem {
                base,
                index,
                scale,
                disp,
            } => Op::Mem(self.ea_addr(base, index, scale, disp)),
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
        if trace_at(start) {
            self.trace_here(start);
        }
        if cover_range().is_some_and(|(lo, hi)| (lo..hi).contains(&start)) {
            self.cover_here(start);
        }
        if self.mem.code_dirty {
            self.flush_code_caches();
        }
        let i = self.block_index(start);
        let ops = self.icache.blocks[i].ops.clone();
        if ops.is_empty() {
            return None;
        }
        self.icache.runs += 1;
        let n = ops.len().min(limit as usize);
        let covered = cover_range().is_some_and(|(lo, hi)| (lo..hi).contains(&start));
        if self.jit.enabled && n == ops.len() && !trace_at(start) && !covered {
            if let Some(r) = self.run_compiled(i, start, limit) {
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
                    stop @ (Stop::Fault(_) | Stop::DivideError(_) | Stop::InvalidOpcode { .. }) => {
                        self.raise_hardware(stop, b.start, saved)
                    }
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
    fn run_compiled(&mut self, i: usize, start: u32, limit: u64) -> Option<(u64, Res<()>)> {
        let current = self.jit.generation;
        let b = &mut self.icache.blocks[i];
        if b.jit_gen != current || b.code == 0 {
            // Rebuilt after being evicted: its compiled code is still there.
            if let Some(f) = self.jit.compiled_for(start, b.ops.len()) {
                b.code = f as usize;
                b.jit_gen = current;
                return self.run_compiled(i, start, limit);
            }
            b.hits += 1;
            if b.hits < super::jit::HOT {
                return None;
            }
            b.hits = 0;
            let ops = b.ops.clone();
            let f = match self.jit.compile(ops) {
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
        self.jit_chain = self.jit.chain.as_ptr();
        // SAFETY: `code` was produced by `jit.compile` for this block in the current
        // generation; it only touches this emulator through the pointer it is given.
        let f: super::jit::BlockFn =
            unsafe { std::mem::transmute::<usize, super::jit::BlockFn>(code) };
        let r = unsafe { f(self, limit) };
        self.jit.runs += 1;
        let count = r & 0xFFFF_FFFF;
        if self.jit.stats && r >> 32 == 0 {
            let eip = self.eip;
            let set = block_set(eip);
            let b = self.icache.blocks[set..set + WAYS].iter().find(|b| b.start == eip);
            let why = if eip.wrapping_sub(HLE_BASE) < HLE_END - HLE_BASE {
                1 // an API stub
            } else if b.is_some_and(|b| b.ops.is_empty()) {
                2 // an instruction blocks do not handle
            } else if b.is_some_and(|b| b.code != 0 && b.jit_gen == self.jit.generation) {
                if limit.saturating_sub(count) < b.map_or(0, |b| b.ops.len() as u64) { 0 } else { 4 }
            } else {
                3 // not compiled (yet)
            };
            self.exit_why[why] += 1;
            if why == 2 {
                let mut k = String::new();
                let mut a = eip;
                for _ in 0..4 {
                    let b = self.mem.read_u8(a).unwrap_or(0);
                    k.push_str(&format!("{b:02X}"));
                    a += 1;
                    if !matches!(b, 0x66 | 0xF2 | 0xF3 | 0x0F) {
                        break;
                    }
                }
                if matches!(k.as_str(), "F7" | "F6" | "FF" | "FE" | "D9" | "DB" | "DD" | "DF" | "D8" | "DA" | "DC" | "DE" | "C1" | "D3" | "0FAE") || k.ends_with("0F") {
                    let m = self.mem.read_u8(a).unwrap_or(0);
                    k.push_str(&format!("/{}", (m >> 3) & 7));
                }
                *self.jit.helper_kinds.entry(format!("undecodable {k}")).or_insert(0) += 1;
            }
        }
        self.steps += count;
        self.recent[(self.steps & 15) as usize] = start;
        if r >> 32 != 0 {
            let stop = self.jit_stop.take().unwrap_or(Stop::Halt);
            return Some((count, Err(stop)));
        }
        Some((count, Ok(())))
    }

    /// XP_TRACE_AT: the registers, the top of the stack (return address, then the first
    /// arguments) and the dword ecx points to where a traced block starts.
    #[cold]
    fn trace_here(&self, at: u32) {
        let r = |a: u32| self.mem.read(a, 4).unwrap_or(0xDEAD_DEAD);
        let esp = self.regs[ESP];
        let regs: Vec<String> = self.regs.iter().map(|v| format!("{v:#x}")).collect();
        let stack: Vec<String> = (0..4).map(|i| format!("{:#x}", r(esp.wrapping_add(4 * i)))).collect();
        // (*ecx: the vtable, when ecx is a C++ `this`.)
        eprintln!(
            "[trace] {at:#x} regs [{}] stack [{}] *ecx {:#x}",
            regs.join(" "),
            stack.join(" "),
            r(self.regs[1])
        );
    }

    /// Cached code was written to: throw away everything made from the old bytes, the decoded
    /// instructions and blocks and the compiled code (which chained blocks jump into directly).
    #[cold]
    fn flush_code_caches(&mut self) {
        self.icache.clear();
        self.mem.forget_code_pages();
        self.jit.reset();
    }

    /// XP_COVER: report the first run of each block in the range, with the time (debugging).
    #[cold]
    fn cover_here(&self, at: u32) {
        thread_local! {
            static SEEN: std::cell::RefCell<std::collections::HashSet<u32>> = Default::default();
        }
        if SEEN.with(|s| s.borrow_mut().insert(at)) {
            eprintln!("[cover] {:.1} {at:x}", self.win.elapsed_ms() as f64 / 1000.0);
        }
    }

    /// The entry holding the block that starts at `start`, building it (in place of the set's
    /// least recently run block) if there is none.
    #[inline(always)]
    fn block_index(&mut self, start: u32) -> usize {
        let set = block_set(start);
        let clock = self.icache.clock.wrapping_add(1);
        self.icache.clock = clock;
        let mut victim = set;
        let mut oldest = 0;
        for i in set..set + WAYS {
            let b = &mut self.icache.blocks[i];
            if b.start == start {
                b.used = clock;
                return i;
            }
            let age = if b.start == EMPTY { u32::MAX } else { clock.wrapping_sub(b.used) };
            if age >= oldest {
                (victim, oldest) = (i, age);
            }
        }
        self.build_block(victim, start);
        self.icache.blocks[victim].used = clock;
        victim
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
            let Some((len, op)) = decode(&buf[..n], at) else {
                // Not decoded here: the block ends with it, run by the full interpreter
                // (which leaves eip wherever the instruction goes).
                ops.push(BlockOp {
                    op: Dop::Unsupported,
                    start: at,
                    next: at,
                });
                break;
            };
            let next = at.wrapping_add(len as u32);
            ops.push(BlockOp {
                op,
                start: at,
                next,
            });
            if ends_block(&op) || next.wrapping_sub(HLE_BASE) < HLE_END - HLE_BASE {
                break;
            }
            at = next;
        }
        self.icache.built += 1;
        self.icache.blocks[i] = Block {
            start,
            ops: ops.into(),
            hits: 0,
            code: 0,
            jit_gen: 0,
            used: 0,
        };
    }

    /// Run the instruction at `start` through the cache: decoded once, then reused.
    #[inline(always)]
    pub(crate) fn step_cached(&mut self, start: u32) -> Res<()> {
        if self.mem.code_dirty {
            self.flush_code_caches();
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
        self.icache.entries[i] = Entry {
            eip: start,
            len,
            op,
        };
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
                let d = if self.eflags & DF != 0 { n.wrapping_neg() } else { n };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::{EBX, ESI, Stop};

    /// Every instruction the block decoder accepts runs the same through `exec_dop` as through
    /// the full interpreter: random instruction bytes, registers, flags and memory, compared
    /// afterwards (registers, flags, eip, the memory window, and whether it faulted).
    #[test]
    fn decoded_instructions_match_the_full_interpreter() {
        const CODE: u32 = 0x1000;
        const DATA: u32 = 0x10_0000;
        let mut x = 0x9E37_79B9u32;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut tried = 0;
        let mut mismatches = Vec::new();
        let (mut a, mut b) = (Emu::new(), Emu::new());
        for e in [&mut a, &mut b] {
            e.mem.map(CODE, 0x1000);
            e.mem.map(DATA, 0x1_0000);
        }
        // (Far fewer unoptimized: the full run takes minutes in a debug build.)
        let tries = if cfg!(debug_assertions) { 30_000 } else { 300_000 };
        for _ in 0..tries {
            let mut bytes = [0u8; 16];
            for b in bytes.iter_mut() {
                *b = rnd() as u8;
            }
            // Mostly small displacements so memory operands land in the mapped window.
            if rnd() % 4 != 0 {
                for b in bytes[2..8].iter_mut() {
                    if rnd() % 2 == 0 {
                        *b = 0;
                    }
                }
            }
            // Two-byte and operand-size forms come up rarely at random: make some.
            match rnd() % 8 {
                0 => {
                    const OPS: [u8; 7] = [0xAF, 0xB6, 0xB7, 0xBE, 0xBF, 0x80, 0x90];
                    let o = OPS[(rnd() % 7) as usize];
                    bytes[0] = 0x0F;
                    bytes[1] = if o >= 0x80 { o + (rnd() % 16) as u8 } else { o };
                }
                1 => bytes[0] = 0x66,
                _ => {}
            }
            let Some((len, op)) = decode(&bytes, CODE) else { continue };
            if op == Dop::Unsupported {
                continue;
            }
            tried += 1;
            let mut regs = [0u32; 8];
            for r in regs.iter_mut() {
                *r = DATA + 0x8000 + (rnd() % 0x1000);
                if rnd() % 3 == 0 {
                    *r = rnd(); // arbitrary values too (faults compared as well)
                }
            }
            regs[ESP as usize] = DATA + 0xC000;
            let flags = (rnd() & 0x8D5) | 2 | (rnd() & 0x400);
            let mem_seed = rnd();
            let mut fpu_st = [0f64; 8];
            for v in fpu_st.iter_mut() {
                *v = match rnd() % 4 {
                    0 => (rnd() as i32) as f64,
                    1 => (rnd() as i32) as f64 / 7.0,
                    2 => (rnd() % 100) as f64 - 50.5,
                    _ => f32::from_bits(rnd()) as f64,
                };
            }
            let fpu_top = (rnd() % 8) as usize;
            let setup = |e: &mut Emu| {
                e.mem.write_bytes(CODE, &bytes).unwrap();
                let mut y = mem_seed;
                for i in 0..0x4000 {
                    y = y.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    e.mem.write(DATA + 4 * i, 4, y).unwrap();
                }
                e.regs = regs;
                e.eflags = flags;
                e.eip = CODE;
                e.fpu.st = fpu_st;
                e.fpu.top = fpu_top;
                e.fpu.cc = 0;
            };
            setup(&mut a);
            let ra = a.exec_dop(op, CODE, CODE + len as u32);
            setup(&mut b);
            let rb = b.step_decoded(CODE);
            let state = |e: &Emu, r: &Res<()>| {
                (
                    r.is_ok(),
                    if r.is_ok() { e.regs } else { [0; 8] },
                    if r.is_ok() { e.eflags & 0x8D5 } else { 0 },
                    if r.is_ok() { e.eip } else { 0 },
                    e.mem.read_bytes(DATA, 0x1_0000).unwrap(),
                    if r.is_ok() { e.fpu.st.map(f64::to_bits) } else { [0; 8] },
                    if r.is_ok() { (e.fpu.top, e.fpu.cc) } else { (0, 0) },
                )
            };
            if state(&a, &ra) != state(&b, &rb) && mismatches.len() < 20 {
                let (sa, sb) = (state(&a, &ra), state(&b, &rb));
                mismatches.push(format!(
                    "{:02x?} {op:?}: ok {}/{} regs {:x?}/{:x?} flags {:#x}/{:#x} eip {:#x}/{:#x} mem same {} fpu same {} top/cc {:?}/{:?}",
                    &bytes[..len as usize], sa.0, sb.0, sa.1, sb.1, sa.2, sb.2, sa.3, sb.3, sa.4 == sb.4, sa.5 == sb.5, sa.6, sb.6
                ));
            }
        }
        assert!(tried > tries / 4, "only {tried} decodable instructions tried");
        assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    }

    /// The instructions added for blocks (8-bit immediates, setcc, cdq, the moffs forms, 16-bit
    /// mov and ALU, not/neg, test with an immediate, movsb/movsd both ways) give the same
    /// registers, flags and memory compiled as interpreted: a loop runs them until it is hot.
    #[test]
    fn added_instructions_compile_like_the_interpreter() {
        const CODE: u32 = 0x1000;
        const M: u32 = 0x3000;
        const SRC: u32 = 0x4000;
        const DST: u32 = 0x5000;
        let le = |v: u32| v.to_le_bytes().to_vec();
        let code: Vec<u8> = [
            vec![0xBE], le(SRC), vec![0xBF], le(DST),  // mov esi, SRC; mov edi, DST
            vec![0xB0, 0x7F, 0xB4, 0x80],              // mov al, 0x7F; mov ah, 0x80
            vec![0xC6, 0x05], le(M), vec![0x5A],       // mov byte [M], 0x5A
            vec![0x0F, 0x9C, 0xC1],                    // setl cl
            vec![0x0F, 0x94, 0x05], le(M + 1),         // sete byte [M+1]
            vec![0x99],                                // cdq
            vec![0x66, 0x8B, 0x1D], le(M + 4),         // mov bx, [M+4]
            vec![0x66, 0x89, 0x1D], le(M + 8),         // mov [M+8], bx
            vec![0x66, 0x89, 0xD9],                    // mov cx, bx
            vec![0x66, 0x03, 0x05], le(M + 4),         // add ax, [M+4]
            vec![0x66, 0x83, 0xC1, 0xF0],              // add cx, -16
            vec![0xA1], le(M + 12),                    // mov eax, [M+12]
            vec![0xF7, 0xD8],                          // neg eax
            vec![0xA3], le(M + 16),                    // mov [M+16], eax
            vec![0xF7, 0xD3],                          // not ebx
            vec![0xF6, 0xC3, 0x81],                    // test bl, 0x81
            vec![0x0F, 0x9F, 0x05], le(M + 2),         // setg byte [M+2]
            vec![0xF7, 0x05], le(M + 4), le(0xF0F),    // test dword [M+4], 0xF0F
            vec![0x0F, 0x92, 0x05], le(M + 3),         // setb byte [M+3]
            vec![0xF7, 0x1D], le(M + 20),              // neg dword [M+20]
            vec![0xA5, 0xA4],                          // movsd; movsb
            vec![0xFD, 0xA5, 0xA4, 0xFC],              // std; movsd; movsb; cld
            vec![0xA5],                                // movsd
            vec![0x4D, 0x0F, 0x85],                    // dec ebp; jnz to the top
        ]
        .concat();
        let mut code = code;
        let back = -(code.len() as i32 + 4);
        code.extend(le(back as u32));
        code.push(0xF4);
        let run = |jit: bool, seed: u32| {
            let mut e = Emu::new();
            e.jit.enabled &= jit;
            for (a, n) in [(CODE, 0x1000), (M, 0x1000), (SRC, 0x1000), (DST, 0x1000)] {
                e.mem.map(a, n);
            }
            e.mem.write_bytes(CODE, &code).unwrap();
            let mut x = seed;
            for i in 0..0x400 {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                for base in [M, SRC] {
                    e.mem.write(base + 4 * i, 4, x ^ base).unwrap();
                }
            }
            e.regs = [seed, 3, 0x8000_0000, seed >> 3, 0x2000, 40, 0, 0];
            e.eip = CODE;
            assert_eq!(e.run(100_000), Stop::Halt);
            if jit {
                assert!(e.jit.compiled > 0, "the loop was compiled");
            }
            let mem: Vec<u8> = [M, SRC, DST]
                .iter()
                .flat_map(|&a| e.mem.read_bytes(a, 0x200).unwrap())
                .collect();
            (e.regs, e.eflags & 0x8D5, mem)
        };
        for seed in [0, 1, 0x8000_0000, 0x1234_5678, 0xFFFF_FFFF] {
            assert_eq!(run(true, seed), run(false, seed), "seed {seed:#x}");
        }
    }

    #[test]
    fn decodes_operands_and_absolute_targets() {
        // mov eax, [ebx+esi*4+0x10]
        assert_eq!(
            decode(&[0x8B, 0x44, 0xB3, 0x10], 0),
            Some((
                4,
                Dop::MovRRm {
                    reg: 0,
                    rm: Ea::Mem {
                        base: 3,
                        index: 6,
                        scale: 2,
                        disp: 0x10
                    },
                    bits: 32
                }
            ))
        );
        // jne -2 at 0x1000: back to itself
        assert_eq!(
            decode(&[0x75, 0xFE], 0x1000),
            Some((
                2,
                Dop::Jcc {
                    cc: 5,
                    target: 0x1000
                }
            ))
        );
        // call +0x100 at 0x2000
        assert_eq!(
            decode(&[0xE8, 0x00, 0x01, 0, 0], 0x2000),
            Some((5, Dop::Call(0x2105)))
        );
        // [disp32] with no base
        assert_eq!(
            decode(&[0x8B, 0x05, 0x78, 0x56, 0x34, 0x12], 0).map(|d| d.1),
            Some(Dop::MovRRm {
                reg: 0,
                rm: Ea::Mem {
                    base: NONE,
                    index: NONE,
                    scale: 0,
                    disp: 0x1234_5678
                },
                bits: 32
            })
        );
        assert_eq!(
            decode(&[0x66, 0x89, 0xC8], 0),
            Some((
                3,
                Dop::MovRmR {
                    rm: Ea::Reg(0),
                    reg: 1,
                    bits: 16
                }
            )),
            "operand-size prefix: the 16-bit form"
        );
        assert_eq!(
            decode(&[0xF3, 0xA5], 0),
            None,
            "rep: left to the interpreter"
        );
        assert_eq!(
            decode(&[0xD9, 0xE8], 0),
            Some((
                2,
                Dop::Fpu {
                    opc: 0xD9,
                    reg: 5,
                    rm: Ea::Reg(0)
                }
            )),
            "fld1, ModRM decoded"
        );
        assert_eq!(decode(&[0x8B], 0), None, "truncated");
    }

    /// Tiny random generator (no external crates in tests).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
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
            if r.next().is_multiple_of(2) {
                vec![0xC0 | (reg << 3) | dst_reg(r)]
            } else {
                let base = r.pick(&[3u8, 6]);
                vec![0x40 | (reg << 3) | base, (r.next() % 0x7C) as u8 & !3]
            }
        };
        match r.next() % 18 {
            15..=17 => {
                // x87: memory forms at [ebx/esi + d8] and register forms
                let mem = |r: &mut Rng, op: u8| {
                    vec![
                        0x40 | (op << 3) | r.pick(&[3u8, 6]),
                        (r.next() % 0x78) as u8 & !7,
                    ]
                };
                match r.next() % 9 {
                    0 => {
                        out.push(r.pick(&[0xD9u8, 0xDD]));
                        let op = r.pick(&[0u8, 2, 3]);
                        out.extend(mem(r, op));
                    }
                    1 => {
                        out.push(r.pick(&[0xD8u8, 0xDC]));
                        let op = r.pick(&[0u8, 1, 4, 5, 6, 7]);
                        out.extend(mem(r, op));
                    }
                    2 | 3 => {
                        let opc = r.pick(&[0xD8u8, 0xDC, 0xDE]);
                        let op = r.pick(&[0u8, 1, 4, 5, 6, 7]);
                        out.extend([opc, 0xC0 | (op << 3) | (r.next() % 8) as u8]);
                    }
                    4 => out.extend([0xD9, 0xC0 | (r.next() % 8) as u8]), // fld st(i)
                    5 => out.extend([0xD9, 0xC8 | (r.next() % 8) as u8]), // fxch
                    6 => out.extend([0xDD, r.pick(&[0xD0u8, 0xD8]) | (r.next() % 8) as u8]), // fst/fstp st(i)
                    7 => match r.next() % 4 {
                        0 => out.extend([0xD9, r.pick(&[0xE0u8, 0xE1, 0xE8, 0xEE])]),
                        1 => {
                            // fcom / fcomp m32 / m64, then fnstsw ax
                            out.push(r.pick(&[0xD8u8, 0xDC]));
                            let op = r.pick(&[2u8, 3]);
                            out.extend(mem(r, op));
                            out.extend([0xDF, 0xE0]);
                        }
                        2 => {
                            out.extend([0xD8, r.pick(&[0xD0u8, 0xD8]) | (r.next() % 8) as u8]);
                            out.extend([0xDF, 0xE0]);
                        }
                        _ => {
                            out.push(r.pick(&[0xDBu8, 0xDF])); // fild m32 / m16
                            out.extend(mem(r, 0));
                        }
                    },
                    _ => {
                        // a value to work with: fild-free, so load a float from memory
                        out.push(0xDD);
                        out.extend(mem(r, 0));
                    }
                }
            }
            12 => {
                // 8-bit mov in either direction
                out.push(r.pick(&[0x88u8, 0x8A]));
                let reg = r.pick(&[0u8, 1, 2, 4, 5, 6]); // al cl dl ah ch dh
                out.extend(modrm_dst(r, reg));
            }
            13 => {
                // 8-bit alu with an immediate, or test r/m8, r8
                if r.next().is_multiple_of(2) {
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
                if r.next().is_multiple_of(2) {
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
        e.mem
            .write_bytes(0x1000 + code.len() as u32, &[0xF4])
            .unwrap(); // hlt
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
            assert_eq!(
                a.mem.read_bytes(0x10000, 0x400).unwrap(),
                b.mem.read_bytes(0x10000, 0x400).unwrap(),
                "memory (seed {seed})"
            );
            assert_eq!(
                a.mem.read_bytes(0x17F00, 0x100).unwrap(),
                b.mem.read_bytes(0x17F00, 0x100).unwrap(),
                "stack (seed {seed})"
            );
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
                assert_eq!(
                    a.fpu.top, b.fpu.top,
                    "x87 stack top (seed {seed}, pass {pass})"
                );
                assert_eq!(
                    a.fpu.st.map(f64::to_bits),
                    b.fpu.st.map(f64::to_bits),
                    "x87 registers (seed {seed}, pass {pass})"
                );
            }
            assert_eq!(a.steps, b.steps, "instruction count (seed {seed})");
            assert_eq!(
                a.mem.read_bytes(0x10000, 0x400).unwrap(),
                b.mem.read_bytes(0x10000, 0x400).unwrap(),
                "memory (seed {seed})"
            );
            assert_eq!(
                a.mem.read_bytes(0x17F00, 0x100).unwrap(),
                b.mem.read_bytes(0x17F00, 0x100).unwrap(),
                "stack (seed {seed})"
            );
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
        assert_eq!(
            e.regs[EAX], 2,
            "the new instruction ran, not the cached one"
        );
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
