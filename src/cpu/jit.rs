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
        if c.eip == start && c.len == len as u32 { self.chain_fn[i] } else { None }
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
mod codegen {
    use dynasmrt::aarch64::Aarch64Relocation;
    use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, VecAssembler, dynasm};

    use super::*;

    type Asm = VecAssembler<Aarch64Relocation>;

    thread_local! {
        /// Whether the instruction being compiled must produce its flags (see `flag_needs`).
        static NEED_FLAGS: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    }

    fn need_flags() -> bool {
        NEED_FLAGS.with(|n| n.get())
    }

    const ESP: u8 = 4;
    const EBP: u8 = 5;
    /// The flags `alu` writes: CF PF ZF SF OF.
    const ARITH_FLAGS: u32 = 0x8C5;

    /// Host register holding guest register `r`.
    fn g(r: u8) -> u8 {
        19 + r
    }

    fn mov32(a: &mut Asm, reg: u8, v: u32) {
        let (lo, hi) = (v & 0xFFFF, v >> 16);
        dynasm!(a ; .arch aarch64 ; movz W(reg), #lo);
        if hi != 0 {
            dynasm!(a ; .arch aarch64 ; movk W(reg), #hi, lsl #16);
        }
    }

    fn mov64(a: &mut Asm, reg: u8, v: u64) {
        dynasm!(a ; .arch aarch64 ; movz X(reg), #(v & 0xFFFF) as u32);
        for s in [16u32, 32, 48] {
            let part = ((v >> s) & 0xFFFF) as u32;
            if part != 0 {
                dynasm!(a ; .arch aarch64 ; movk X(reg), #part, lsl #s);
            }
        }
    }

    /// Effective address of a memory operand into w0.
    fn ea(a: &mut Asm, base: u8, index: u8, scale: u8, disp: u32) {
        mov32(a, 0, disp);
        if base != 8 {
            dynasm!(a ; .arch aarch64 ; add w0, w0, W(g(base)));
        }
        if index != 8 {
            let s = scale as u32;
            dynasm!(a ; .arch aarch64 ; add w0, w0, W(g(index)), lsl #s);
        }
    }

    /// x10 = host address of the page holding w0, or go to `slow`; w11 = offset in the page.
    /// `size` bytes must not cross into the next page.
    fn page(a: &mut Asm, size: u32, slow: DynamicLabel) {
        let limit = 4096 - size;
        dynasm!(a ; .arch aarch64
            ; lsr w9, w0, #12
            ; ldr x10, [x15, x9, lsl #3]
            ; cbz x10, =>slow
            ; and w11, w0, #0xFFF
            ; cmp w11, #limit
            ; b.hi =>slow
        );
    }

    /// Load 8 bytes at w0 into x1.
    fn load64(a: &mut Asm, slow: DynamicLabel) {
        page(a, 8, slow);
        dynasm!(a ; .arch aarch64 ; ldr x1, [x10, x11]);
    }

    /// Store x1 (8 bytes) at w0.
    fn store64(a: &mut Asm, slow: DynamicLabel) {
        dynasm!(a ; .arch aarch64
            ; lsr w9, w0, #12
            ; ldrb w12, [x14, x9]
            ; cbnz w12, =>slow
        );
        page(a, 8, slow);
        dynasm!(a ; .arch aarch64 ; str x1, [x10, x11]);
    }

    // ---- x87: the register stack lives in emu.fpu (x16 = &st[0], x17 = top) ----

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
    fn fpu(a: &mut Asm, opc: u8, reg: u8, rm: Ea, slow: DynamicLabel) -> bool {
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

    /// Load `size` (1, 2, 4) bytes at w0 into W(dst), zero-extended.
    fn load(a: &mut Asm, size: u32, dst: u8, slow: DynamicLabel) {
        page(a, size, slow);
        match size {
            1 => dynasm!(a ; .arch aarch64 ; ldrb W(dst), [x10, x11]),
            2 => dynasm!(a ; .arch aarch64 ; ldrh W(dst), [x10, x11]),
            _ => dynasm!(a ; .arch aarch64 ; ldr W(dst), [x10, x11]),
        }
    }

    /// Store W(src) (32 bits) at w0. Pages holding cached code take the slow path.
    fn store32(a: &mut Asm, src: u8, slow: DynamicLabel) {
        dynasm!(a ; .arch aarch64
            ; lsr w9, w0, #12
            ; ldrb w12, [x14, x9]
            ; cbnz w12, =>slow
        );
        page(a, 4, slow);
        dynasm!(a ; .arch aarch64 ; str W(src), [x10, x11]);
    }

    /// Store the low 16 bits of W(src) at w0, as `store32`.
    fn store16(a: &mut Asm, src: u8, slow: DynamicLabel) {
        dynasm!(a ; .arch aarch64
            ; lsr w9, w0, #12
            ; ldrb w12, [x14, x9]
            ; cbnz w12, =>slow
        );
        page(a, 2, slow);
        dynasm!(a ; .arch aarch64 ; strh W(src), [x10, x11]);
    }

    /// After an arithmetic/logic instruction set NZCV for result w2: put the new EFLAGS value
    /// (CF PF ZF SF OF as `alu` computes them) in w3. `borrow`: CF is ARM's C inverted (sub).
    /// `keep_cf`: inc/dec, which leave CF alone.
    /// `byte_high`: an 8-bit operation done in the top byte of w2 (see `alu8`).
    fn flags(a: &mut Asm, borrow: bool, keep_cf: bool) {
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
    fn alu(a: &mut Asm, op: u8) -> Option<bool> {
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

    /// Write all guest state held in host registers back to `Emu`.
    fn sync_out(a: &mut Asm) {
        for r in 0..8u8 {
            let off = OFF_REGS + 4 * r as u32;
            dynasm!(a ; .arch aarch64 ; str W(19 + r), [x28, #off]);
        }
        dynasm!(a ; .arch aarch64 ; str w27, [x28, #OFF_EFLAGS]);
    }

    /// Reload guest state (and the page-table pointers) from `Emu`.
    fn sync_in(a: &mut Asm) {
        for r in 0..8u8 {
            let off = OFF_REGS + 4 * r as u32;
            dynasm!(a ; .arch aarch64 ; ldr W(19 + r), [x28, #off]);
        }
        dynasm!(a ; .arch aarch64
            ; ldr w27, [x28, #OFF_EFLAGS]
            ; ldr x15, [x28, #OFF_PAGES]
            ; ldr x14, [x28, #OFF_CODE_PAGES]
        );
    }

    /// x86 condition `cc` from the flags in w27, as 0/1 in w0.
    fn cond(a: &mut Asm, cc: u8) {
        match cc >> 1 {
            0 => dynasm!(a ; .arch aarch64 ; ubfx w0, w27, #11, #1),
            1 => dynasm!(a ; .arch aarch64 ; and w0, w27, #1),
            2 => dynasm!(a ; .arch aarch64 ; ubfx w0, w27, #6, #1),
            3 => {
                dynasm!(a ; .arch aarch64 ; and w0, w27, #1 ; ubfx w1, w27, #6, #1 ; orr w0, w0, w1)
            }
            4 => dynasm!(a ; .arch aarch64 ; ubfx w0, w27, #7, #1),
            5 => dynasm!(a ; .arch aarch64 ; ubfx w0, w27, #2, #1),
            6 => {
                dynasm!(a ; .arch aarch64 ; ubfx w1, w27, #7, #1 ; ubfx w2, w27, #11, #1 ; eor w0, w1, w2)
            }
            _ => dynasm!(a ; .arch aarch64
                ; ubfx w1, w27, #7, #1 ; ubfx w2, w27, #11, #1 ; eor w0, w1, w2
                ; ubfx w1, w27, #6, #1 ; orr w0, w0, w1),
        }
        if cc & 1 != 0 {
            dynasm!(a ; .arch aarch64 ; eor w0, w0, #1);
        }
    }

    /// Read an 8-bit operand into W(dst), zero-extended. Registers 4..7 are AH, CH, DH, BH.
    fn read8(a: &mut Asm, rm: Ea, dst: u8, slow: DynamicLabel) {
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
    fn write8(a: &mut Asm, rm: Ea, src: u8, slow: DynamicLabel) {
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
    fn alu8(a: &mut Asm, o: u8, rm: Ea, src: AluSrc, test: bool, slow: DynamicLabel) {
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
    fn alu8_reg(a: &mut Asm, o: u8, reg: u8, rm: Ea, slow: DynamicLabel) {
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
    fn shift_imm(a: &mut Asm, op: u8, rm: Ea, c: u32, slow: DynamicLabel) {
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
    fn shift_cl(a: &mut Asm, op: u8, rm: Ea, slow: DynamicLabel) {
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
    fn imul(a: &mut Asm, reg: u8) {
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
    fn read_rm(a: &mut Asm, rm: Ea, dst: u8, slow: DynamicLabel) {
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

    enum AluSrc {
        Reg(u8),
        Imm(u32),
    }

    /// `rm = rm op src` (or just the flags, for cmp and test).
    fn alu_rm(a: &mut Asm, o: u8, rm: Ea, src: AluSrc, test: bool, slow: DynamicLabel) {
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

    /// Emit the native form of `op`. Returns false (emitting nothing) if there is none.
    /// `next` is the following instruction's address. Control transfers set eip (in w0) and
    /// jump to `exit`.
    fn native(a: &mut Asm, op: &BlockOp, slow: DynamicLabel, exit: DynamicLabel) -> bool {
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

    /// For each instruction: must it produce its flags? Not if a later instruction in the
    /// block overwrites all of CF PF ZF SF OF before anything reads them. Flags are live at the
    /// end of the block, and an instruction run by the interpreter counts as reading them.
    fn flag_needs(ops: &[BlockOp], exit: DynamicLabel) -> Vec<bool> {
        // Which instructions are compiled natively: try them in a scratch assembler.
        let mut scratch = Asm::new(0);
        let (slow, _) = (scratch.new_dynamic_label(), exit);
        let scratch_exit = scratch.new_dynamic_label();
        let native_ok: Vec<bool> = ops
            .iter()
            .map(|op| native(&mut scratch, op, slow, scratch_exit))
            .collect();
        let mut need = vec![true; ops.len()];
        let mut live = true;
        for (i, op) in ops.iter().enumerate().rev() {
            need[i] = live;
            let (reads, writes_all) = if !native_ok[i] {
                (true, false)
            } else {
                match op.op {
                    Dop::AluRmR { op: o, .. }
                    | Dop::AluRRm { op: o, .. }
                    | Dop::AluRmI { op: o, .. } => (o == 2 || o == 3, true),
                    Dop::Test { .. } | Dop::TestI { .. } | Dop::Unary { neg: true, .. } => {
                        (false, true)
                    }
                    Dop::Setcc { .. } => (true, false),
                    Dop::Shift { by_cl: false, .. } => (false, true),
                    Dop::Shift { .. }
                    | Dop::IncDec { .. }
                    | Dop::Imul { .. }
                    | Dop::ImulI { .. }
                    | Dop::Jcc { .. } => (true, false),
                    _ => (false, false),
                }
            };
            if reads {
                live = true;
            } else if writes_all {
                live = false;
            }
        }
        need
    }

    /// Machine code for a block, and the offset of its body (where chained blocks enter, with
    /// the guest state already in registers and the frame already set up).
    ///
    /// Frame: callee-saved registers, then [sp+96] instructions run so far (all chained blocks)
    /// and [sp+104] the budget.
    pub(super) fn block(ops: &[BlockOp]) -> Option<(Vec<u8>, usize)> {
        let mut a = Asm::new(0);
        let exit = a.new_dynamic_label(); // w0 = eip, x13 = instructions run in this block
        let ret_only = a.new_dynamic_label(); // x0 = return value, Emu already up to date
        dynasm!(a ; .arch aarch64
            ; stp x29, x30, [sp, #-112]!
            ; stp x19, x20, [sp, #16]
            ; stp x21, x22, [sp, #32]
            ; stp x23, x24, [sp, #48]
            ; stp x25, x26, [sp, #64]
            ; stp x27, x28, [sp, #80]
            ; mov x28, x0
            ; stp xzr, x1, [sp, #96]
        );
        sync_in(&mut a);
        let body = a.offset().0;
        let need = flag_needs(ops, exit);
        let mut slows = Vec::new();
        let mut ended = false;
        for (i, op) in ops.iter().enumerate() {
            let slow = a.new_dynamic_label();
            let after = a.new_dynamic_label();
            let count = i as u64 + 1;
            let transfer = matches!(
                op.op,
                Dop::Unsupported
                    | Dop::Jcc { .. }
                    | Dop::Jmp(_)
                    | Dop::Call(_)
                    | Dop::Ret(_)
                    | Dop::CallInd(_)
                    | Dop::JmpInd(_)
            );
            if transfer {
                // The exit reads this block's instruction count from x13.
                mov64(&mut a, 13, count);
            }
            NEED_FLAGS.with(|n| n.set(need[i]));
            let done = native(&mut a, op, slow, exit);
            NEED_FLAGS.with(|n| n.set(true));
            if !done {
                dynasm!(a ; .arch aarch64 ; b =>slow);
            }
            dynasm!(a ; .arch aarch64 ; =>after);
            slows.push((slow, after, op as *const BlockOp, count, op.op == Dop::Unsupported));
            if matches!(
                op.op,
                Dop::Unsupported
                    | Dop::Jcc { .. }
                    | Dop::Jmp(_)
                    | Dop::Call(_)
                    | Dop::Ret(_)
                    | Dop::CallInd(_)
                    | Dop::JmpInd(_)
            ) {
                ended = true;
                // If the transfer went through the slow path it has left (see below); the
                // native path has already jumped to `exit`.
                break;
            }
        }
        // Fell off the end of the block (or a final transfer went through the slow path and
        // carried on to the next instruction): continue there.
        let _ = ended;
        let last = ops.len().min(slows.len());
        mov32(&mut a, 0, ops[last - 1].next);
        mov64(&mut a, 13, last as u64);
        dynasm!(a ; .arch aarch64 ; b =>exit);
        // Slow paths: run the instruction in the interpreter.
        for (slow, after, op_ptr, count, interp) in slows {
            dynasm!(a ; .arch aarch64 ; =>slow);
            sync_out(&mut a);
            dynasm!(a ; .arch aarch64 ; mov x0, x28);
            mov64(&mut a, 1, op_ptr as u64);
            mov64(&mut a, 9, jit_step as usize as u64);
            dynasm!(a ; .arch aarch64 ; blr x9);
            if interp {
                // Ends the block wherever it went: on to the next block like any transfer.
                dynasm!(a ; .arch aarch64 ; cmp x0, #1 ; b.hi >leave);
                sync_in(&mut a);
                dynasm!(a ; .arch aarch64 ; ldr w0, [x28, #OFF_EIP]);
                mov64(&mut a, 13, count);
                dynasm!(a ; .arch aarch64 ; b =>exit ; leave:);
            } else {
                dynasm!(a ; .arch aarch64 ; cbnz x0, >leave);
                sync_in(&mut a);
                dynasm!(a ; .arch aarch64 ; b =>after ; leave:);
            }
            // status 1: leave normally; 2: leave flagging emu.jit_stop
            mov64(&mut a, 2, count);
            dynasm!(a ; .arch aarch64
                ; ldr x12, [sp, #96]
                ; add x2, x2, x12
                ; cmp x0, #2
                ; b.ne >plain
                ; orr x2, x2, #0x100000000
                ; plain:
                ; mov x0, x2
                ; b =>ret_only
            );
        }
        // Normal exit: eip in w0, this block's count in x13. Continue straight into the next
        // compiled block if there is one and the budget allows its whole length.
        dynasm!(a ; .arch aarch64
            ; =>exit
            ; ldr x12, [sp, #96]
            ; add x12, x12, x13
            ; str x12, [sp, #96]
            ; movz w9, #0x79B1
            ; movk w9, #0x9E37, lsl #16
            ; mul w9, w0, w9
            ; lsr w9, w9, #CHAIN_SHIFT
            ; ldr x10, [x28, #OFF_CHAIN]
            ; add x10, x10, x9, lsl #4
            ; ldr w11, [x10]
            ; cmp w11, w0
            ; b.ne >nochain
            ; ldr w11, [x10, #4]
            ; add x11, x12, x11
            ; ldr x9, [sp, #104]
            ; cmp x11, x9
            ; b.hi >nochain
            ; ldr x9, [x10, #8]
        );
        let stats = std::env::var_os("XP_BLOCK_STATS").is_some();
        if stats {
            dynasm!(a ; .arch aarch64 ; ldr x11, [x28, #OFF_EXITS] ; add x11, x11, #1 ; str x11, [x28, #OFF_EXITS]);
        }
        dynasm!(a ; .arch aarch64 ; br x9 ; nochain: ; str w0, [x28, #OFF_EIP]);
        if stats {
            dynasm!(a ; .arch aarch64 ; ldr x11, [x28, #OFF_EXITS + 8] ; add x11, x11, #1 ; str x11, [x28, #OFF_EXITS + 8]);
        }
        dynasm!(a ; .arch aarch64
        );
        sync_out(&mut a);
        dynasm!(a ; .arch aarch64 ; mov x0, x12 ; =>ret_only
            ; ldp x19, x20, [sp, #16]
            ; ldp x21, x22, [sp, #32]
            ; ldp x23, x24, [sp, #48]
            ; ldp x25, x26, [sp, #64]
            ; ldp x27, x28, [sp, #80]
            ; ldp x29, x30, [sp], #112
            ; ret
        );
        a.finalize().ok().map(|code| (code, body))
    }
}

#[cfg(not(target_arch = "aarch64"))]
mod codegen {
    pub(super) fn block(_: &[super::BlockOp]) -> Option<(Vec<u8>, usize)> {
        None
    }
}
