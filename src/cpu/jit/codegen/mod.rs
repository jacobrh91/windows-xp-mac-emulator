//! The ARM64 code generator: guest registers live in host registers for the whole block, and
//! `block` assembles a block's instructions around the shared pieces here.

mod alu;
mod fpu;
mod native;

use dynasmrt::aarch64::Aarch64Relocation;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, VecAssembler, dynasm};

use alu::*;
use fpu::fpu;
use native::native;

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
        slows.push((
            slow,
            after,
            op as *const BlockOp,
            count,
            op.op == Dop::Unsupported,
        ));
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
