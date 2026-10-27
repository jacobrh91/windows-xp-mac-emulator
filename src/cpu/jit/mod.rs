//! x86 -> ARM64 translator for hot blocks.
//!
//! Blocks from the instruction cache (`icache`) that have run a few times are compiled to
//! native code. Guest registers live in host registers for the whole block (EAX..EDI in
//! w19..w26, EFLAGS in w27, the `Emu` pointer in x28), and guest memory is reached through
//! the same page table the interpreter uses (x15), with a check for pages holding cached code
//! (x14).
//!
//! Correctness rules, so that compiled and interpreted execution are indistinguishable:
//! - Each instruction does all its memory accesses before it changes any register or flag.
//!   If an access cannot take the fast path (unmapped page, crossing a page, a page holding
//!   cached code), the instruction is redone by `jit_step`, the interpreter's implementation,
//!   which also turns faults into Windows exceptions exactly as before.
//! - Flags are computed bit for bit as `Emu::alu` does.
//! - Instructions without a native translation call `jit_step` too, so any block compiles.
//!
//! `XP_NO_JIT=1` turns it off.

use std::ffi::c_void;

use super::icache::{BlockOp, Dop, Ea};
use super::{Emu, Stop};

/// Compile a block once it has run this many times.
pub(crate) const HOT: u32 = 16;
/// Executable memory reserved for compiled code.
const CODE_BYTES: usize = 128 << 20;

/// Compiled block: `fn(emu, budget) -> instructions run | (1 << 32 if emu.jit_stop holds a
/// Stop)`. It may continue into further compiled blocks (chaining) while the instructions run
/// stay within `budget`.
pub(crate) type BlockFn = unsafe extern "C" fn(*mut Emu, u64) -> u64;

/// Slots in the chaining table.
const CHAIN_SLOTS: usize = 1 << 20;
const CHAIN_BITS: u32 = CHAIN_SLOTS.trailing_zeros();
const CHAIN_SHIFT: u32 = 32 - CHAIN_BITS;

/// Where compiled code continues into another compiled block: its guest address, length and
/// the address of its body (just after its register loading).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ChainEntry {
    eip: u32,
    len: u32,
    body: u64,
}

pub struct Jit {
    pub(crate) enabled: bool,
    mem: *mut u8,
    used: usize,
    /// Bumped whenever compiled code is thrown away (blocks compare it to their own).
    pub(crate) generation: u32,
    pub compiled: u64,
    /// Instructions compiled blocks handed to the interpreter, and compiled-block runs.
    pub helper_calls: u64,
    pub runs: u64,
    /// Helper calls by operation kind (statistics).
    pub helper_kinds: std::collections::HashMap<String, u64>,
    pub(crate) stats: bool,
    /// Chaining table, read by compiled code (see `codegen::block`), and each entry's
    /// compiled function.
    pub(crate) chain: Vec<ChainEntry>,
    chain_fn: Vec<Option<BlockFn>>,
    /// Operation lists that compiled code points into: kept until the code is thrown away.
    keep: Vec<std::rc::Rc<[BlockOp]>>,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

impl Jit {
    pub(crate) fn new() -> Jit {
        let enabled = cfg!(all(target_arch = "aarch64", target_os = "macos"))
            && std::env::var_os("XP_NO_JIT").is_none();
        Jit {
            enabled,
            mem: std::ptr::null_mut(),
            used: 0,
            generation: 1,
            compiled: 0,
            helper_calls: 0,
            runs: 0,
            helper_kinds: Default::default(),
            stats: std::env::var_os("XP_BLOCK_STATS").is_some(),
            chain: vec![
                ChainEntry {
                    eip: u32::MAX,
                    len: 0,
                    body: 0
                };
                CHAIN_SLOTS
            ],
            chain_fn: vec![None; CHAIN_SLOTS],
            keep: Vec::new(),
        }
    }

    /// Forget all compiled code (the block cache was flushed, or the code area is full).
    pub(crate) fn reset(&mut self) {
        self.used = 0;
        self.generation = self.generation.wrapping_add(1).max(1);
        for c in self.chain.iter_mut() {
            c.eip = u32::MAX;
        }
        self.keep.clear();
    }

    /// Copy machine code into executable memory.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn install(&mut self, code: &[u8]) -> Option<BlockFn> {
        const PROT_RWX: i32 = 1 | 2 | 4;
        const MAP_PRIVATE_ANON_JIT: i32 = 0x2 | 0x1000 | 0x800;
        if self.mem.is_null() {
            // SAFETY: an anonymous private mapping; checked for failure below.
            let p = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    CODE_BYTES,
                    PROT_RWX,
                    MAP_PRIVATE_ANON_JIT,
                    -1,
                    0,
                )
            };
            if p as isize == -1 {
                self.enabled = false;
                return None;
            }
            self.mem = p as *mut u8;
        }
        if self.used + code.len() > CODE_BYTES {
            return None; // the caller resets and tries later
        }
        // SAFETY: within the mapping; W^X is toggled for this thread around the copy.
        unsafe {
            let at = self.mem.add(self.used);
            pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(code.as_ptr(), at, code.len());
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(at as *mut c_void, code.len());
            self.used += (code.len() + 15) & !15;
            self.compiled += 1;
            Some(std::mem::transmute::<*mut u8, BlockFn>(at))
        }
    }

    #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
    fn install(&mut self, _: &[u8]) -> Option<BlockFn> {
        None
    }

    /// Code already compiled (this generation) for the `len`-instruction block at `start`.
    #[inline(always)]
    pub(crate) fn compiled_for(&self, start: u32, len: usize) -> Option<BlockFn> {
        let i = chain_slot(start);
        let c = &self.chain[i];
        if c.eip == start && c.len == len as u32 {
            self.chain_fn[i]
        } else {
            None
        }
    }

    /// Compile a block. None if it cannot be (then it keeps running in the interpreter).
    pub(crate) fn compile(&mut self, ops: std::rc::Rc<[BlockOp]>) -> Option<BlockFn> {
        if !self.enabled || ops.is_empty() {
            return None;
        }
        let (code, body) = codegen::block(&ops)?;
        let f = self.install(&code)?;
        let start = ops[0].start;
        self.chain[chain_slot(start)] = ChainEntry {
            eip: start,
            len: ops.len() as u32,
            body: f as usize as u64 + body as u64,
        };
        self.chain_fn[chain_slot(start)] = Some(f);
        self.keep.push(ops);
        Some(f)
    }
}

/// Run one instruction of a compiled block in the interpreter (no native translation, or the
/// fast path could not be used). Returns 0: carry on with the next instruction; 1: control
/// went elsewhere (`emu.eip`); 2: leave with `emu.jit_stop`; 3: cached code was written, leave
/// for the run loop to flush it.
pub(crate) unsafe extern "C" fn jit_step(emu: *mut Emu, op: *const BlockOp) -> u64 {
    // SAFETY: called from compiled code with the emulator that is running it, and a pointer
    // into that block's operation list, which outlives the compiled code.
    let (emu, b) = unsafe { (&mut *emu, &*op) };
    emu.jit.helper_calls += 1;
    // (Instructions blocks do not decode at all are too many to name one by one.)
    if emu.jit.stats && b.op != Dop::Unsupported {
        let kind = match b.op {
            Dop::Fpu {
                opc,
                reg,
                rm: Ea::Mem { .. },
            } => format!("x87 {opc:02X}/{reg} mem"),
            Dop::Fpu {
                opc,
                reg,
                rm: Ea::Reg(i),
            } => format!("x87 {opc:02X}/{reg} st{i}"),
            Dop::Shift {
                op, by_cl, count, ..
            } => format!(
                "shift /{op} {}",
                if by_cl {
                    "cl".to_string()
                } else {
                    count.to_string()
                }
            ),
            other => format!("{other:?}")
                .split([' ', '(', '{'])
                .next()
                .unwrap_or("?")
                .to_string(),
        };
        *emu.jit.helper_kinds.entry(kind).or_insert(0) += 1;
    }
    let saved = (emu.regs, emu.eflags);
    match emu.exec_dop(b.op, b.start, b.next) {
        Ok(()) if emu.mem.code_dirty => 3,
        Ok(()) => (emu.eip != b.next) as u64,
        Err(e) => {
            let r = match e {
                stop @ (Stop::Fault(_) | Stop::DivideError(_) | Stop::InvalidOpcode { .. }) => {
                    emu.raise_hardware(stop, b.start, saved)
                }
                other => Err(other),
            };
            match r {
                Ok(()) => 1,
                Err(stop) => {
                    emu.jit_stop = Some(stop);
                    2
                }
            }
        }
    }
}

/// As `icache::block_slot`; compiled code computes the same (see `codegen::block`).
fn chain_slot(eip: u32) -> usize {
    (eip.wrapping_mul(0x9E37_79B1) >> (32 - CHAIN_BITS)) as usize
}

/// Byte offsets of what compiled code touches in `Emu` (which is `repr(C)`).
pub(crate) const OFF_REGS: u32 = std::mem::offset_of!(Emu, regs) as u32;
pub(crate) const OFF_EIP: u32 = std::mem::offset_of!(Emu, eip) as u32;
pub(crate) const OFF_EFLAGS: u32 = std::mem::offset_of!(Emu, eflags) as u32;
pub(crate) const OFF_PAGES: u32 = std::mem::offset_of!(Emu, jit_pages) as u32;
pub(crate) const OFF_CODE_PAGES: u32 = std::mem::offset_of!(Emu, jit_code_pages) as u32;
pub(crate) const OFF_CHAIN: u32 = std::mem::offset_of!(Emu, jit_chain) as u32;
pub(crate) const OFF_EXITS: u32 = std::mem::offset_of!(Emu, jit_exits) as u32;
pub(crate) const OFF_FPU: u32 = std::mem::offset_of!(Emu, fpu) as u32;
const _: () = assert!(
    OFF_REGS == 0
        && OFF_EIP == 32
        && OFF_EFLAGS == 36
        && OFF_PAGES == 40
        && OFF_CODE_PAGES == 48
        && OFF_CHAIN == 56
);
const _: () = assert!(
    OFF_FPU < 4096
        && std::mem::offset_of!(super::fpu::Fpu, st) == 0
        && std::mem::offset_of!(super::fpu::Fpu, top) == 64
        && std::mem::offset_of!(super::fpu::Fpu, cc) == 72
        && OFF_FPU_CW % 2 == 0
        && OFF_FPU_CW < 128
);
/// The x87 control word, relative to `emu.fpu`.
pub(crate) const OFF_FPU_CW: u32 = std::mem::offset_of!(super::fpu::Fpu, cw) as u32;

#[cfg(target_arch = "aarch64")]
mod codegen;

#[cfg(not(target_arch = "aarch64"))]
mod codegen {
    pub(super) fn block(_: &[super::BlockOp]) -> Option<(Vec<u8>, usize)> {
        None
    }
}
