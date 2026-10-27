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

use super::{Emu, Res, Stop};
use crate::win32::{HLE_BASE, HLE_END};

/// Entries in the table (a power of two).
const SLOTS: usize = 1 << 16;
/// Tag of an empty slot (no instruction lives at the very top of the address space).
const EMPTY: u32 = u32::MAX;

mod debug;
mod decode;
mod exec;
#[cfg(test)]
mod tests;

pub(crate) use debug::{cover_range, trace_at};
pub(crate) use decode::{Dop, Ea};
use decode::{decode, ends_block};

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

/// The first entry of the set a block starting at `eip` belongs to. A multiplicative
/// (Fibonacci) hash: code addresses that differ only in their high bits, as a program's
/// functions do, still spread over the whole table.
fn block_set(eip: u32) -> usize {
    (eip.wrapping_mul(0x9E37_79B1) >> (32 - SET_BITS)) as usize * WAYS
}

impl Emu {
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
            let b = self.icache.blocks[set..set + WAYS]
                .iter()
                .find(|b| b.start == eip);
            let why = if eip.wrapping_sub(HLE_BASE) < HLE_END - HLE_BASE {
                1 // an API stub
            } else if b.is_some_and(|b| b.ops.is_empty()) {
                2 // an instruction blocks do not handle
            } else if b.is_some_and(|b| b.code != 0 && b.jit_gen == self.jit.generation) {
                if limit.saturating_sub(count) < b.map_or(0, |b| b.ops.len() as u64) {
                    0
                } else {
                    4
                }
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
                if matches!(
                    k.as_str(),
                    "F7" | "F6"
                        | "FF"
                        | "FE"
                        | "D9"
                        | "DB"
                        | "DD"
                        | "DF"
                        | "D8"
                        | "DA"
                        | "DC"
                        | "DE"
                        | "C1"
                        | "D3"
                        | "0FAE"
                ) || k.ends_with("0F")
                {
                    let m = self.mem.read_u8(a).unwrap_or(0);
                    k.push_str(&format!("/{}", (m >> 3) & 7));
                }
                *self
                    .jit
                    .helper_kinds
                    .entry(format!("undecodable {k}"))
                    .or_insert(0) += 1;
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

    /// Cached code was written to: throw away everything made from the old bytes, the decoded
    /// instructions and blocks and the compiled code (which chained blocks jump into directly).
    #[cold]
    fn flush_code_caches(&mut self) {
        self.icache.clear();
        self.mem.forget_code_pages();
        self.jit.reset();
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
            let age = if b.start == EMPTY {
                u32::MAX
            } else {
                clock.wrapping_sub(b.used)
            };
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
}
