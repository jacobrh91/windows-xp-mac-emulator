//! Structured exception handling, the 32-bit Windows way.
//!
//! Every function that wants to catch exceptions links an EXCEPTION_REGISTRATION_RECORD
//! {next, handler} onto the list whose head is `fs:[0]`. When an exception happens
//! (a hardware fault, or `RaiseException`, which is what C++ `throw` becomes), the
//! dispatcher builds an EXCEPTION_RECORD and a CONTEXT (the register state) on the guest
//! stack, then calls each handler in turn: `handler(record, frame, context, dispatcher)`.
//! A handler returns 0 ("resume with the context as it now is") or 1 ("not mine"), or it
//! never returns: C++ and `__try` handlers call `RtlUnwind` to run the cleanup code of the
//! frames in between, then jump into their catch block with the stack reset.
//!
//! The dispatcher is a small state machine driven from the guest's own returns (through a
//! hidden stub) instead of host recursion, so a handler that abandons the stack costs
//! nothing and a wait inside a handler stays resumable.

use crate::cpu::{EAX, EBP, EBX, ECX, EDI, EDX, ESI, ESP, Emu, Res, Stop};
use crate::memory::Fault;
use crate::win32::Args;

pub const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const STATUS_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub const STATUS_UNWIND: u32 = 0xC000_0027;

pub const EXCEPTION_NONCONTINUABLE: u32 = 1;
pub const EXCEPTION_UNWINDING: u32 = 2;
const EXCEPTION_EXIT_UNWIND: u32 = 4;

const END_OF_CHAIN: u32 = 0xFFFF_FFFF;
const CONTEXT_FULL: u32 = 0x0001_0007;

// CONTEXT (x86) field offsets.
const CTX_SIZE: u32 = 0x2CC;
const CTX_EDI: u32 = 0x9C;
const CTX_ESI: u32 = 0xA0;
const CTX_EBX: u32 = 0xA4;
const CTX_EDX: u32 = 0xA8;
const CTX_ECX: u32 = 0xAC;
const CTX_EAX: u32 = 0xB0;
const CTX_EBP: u32 = 0xB4;
const CTX_EIP: u32 = 0xB8;
const CTX_EFLAGS: u32 = 0xC0;
const CTX_ESP: u32 = 0xC4;

const REC_SIZE: u32 = 80;

/// One exception being dispatched.
pub struct SehState {
    ctx: u32,
    /// The EXCEPTION_POINTERS passed to unhandled-exception filters.
    pointers: u32,
    rec: u32,
    dispatcher: u32,
    /// Registration record whose handler runs next.
    frame: u32,
    noncontinuable: bool,
    code: u32,
    address: u32,
    /// What to stop with if nothing handles it.
    unhandled: Stop,
}

impl Emu {
    /// A hardware fault from the instruction at `start`. If a handler deals with it, the
    /// guest carries on (and this returns Ok); if not, the original stop is returned.
    pub(crate) fn raise_hardware(
        &mut self,
        stop: Stop,
        start: u32,
        saved: ([u32; 8], u32),
    ) -> Res<()> {
        let (code, params): (u32, Vec<u32>) = match &stop {
            Stop::Fault(Fault(addr)) => (
                STATUS_ACCESS_VIOLATION,
                vec![self.mem.fault_write.get() as u32, *addr],
            ),
            Stop::DivideError(_) => (STATUS_INTEGER_DIVIDE_BY_ZERO, vec![]),
            Stop::InvalidOpcode { .. } => (STATUS_ILLEGAL_INSTRUCTION, vec![]),
            _ => return Err(stop),
        };
        // The context is the state *before* the faulting instruction touched anything.
        self.regs = saved.0;
        self.eflags = saved.1;
        self.eip = start;
        self.start_exception(code, 0, start, &params, stop)
    }

    /// Begin dispatching an exception: build its records on the guest stack and call the
    /// first handler. `Err(unhandled)` means there was nothing to call.
    pub(crate) fn start_exception(
        &mut self,
        code: u32,
        flags: u32,
        address: u32,
        params: &[u32],
        unhandled: Stop,
    ) -> Res<()> {
        // Hardware faults are worth reporting even without tracing (C++ throws are routine).
        let hardware = matches!(code, 0xC000_0005 | 0xC000_001D | 0xC000_0094);
        if self.win.trace || hardware {
            let at = self
                .win
                .locate(address)
                .unwrap_or_else(|| format!("{address:#x}"));
            let bytes: Vec<String> = (0..8)
                .filter_map(|i| self.mem.read_u8(address.wrapping_add(i)).ok())
                .map(|b| format!("{b:02x}"))
                .collect();
            eprintln!(
                "[exception] {code:#x} at {at} (bytes {}) params {params:x?}",
                bytes.join(" ")
            );
            let mut hist: Vec<String> = (1..=16u64)
                .map(|i| self.recent[((self.steps + 16 - i) & 15) as usize])
                .map(|a| self.win.locate(a).unwrap_or_else(|| format!("{a:#x}")))
                .collect();
            hist.reverse();
            eprintln!("[exception]   recent: {}", hist.join(" "));
            // Return addresses on the stack that point into loaded modules.
            let mut shown = 0;
            for i in 0..128u32 {
                let Ok(v) = self.mem.read(self.regs[ESP].wrapping_add(4 * i), 4) else {
                    break;
                };
                if let Some(loc) = self.win.locate(v) {
                    eprintln!("[exception]   stack+{:#04x}: {v:#x} = {loc}", 4 * i);
                    shown += 1;
                    if shown >= 8 {
                        break;
                    }
                }
            }
            eprintln!(
                "[exception]   regs eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esi={:#x} edi={:#x} ebp={:#x} esp={:#x}",
                self.regs[EAX],
                self.regs[ECX],
                self.regs[EDX],
                self.regs[EBX],
                self.regs[ESI],
                self.regs[EDI],
                self.regs[EBP],
                self.regs[ESP]
            );
        }
        let esp0 = self.regs[ESP];
        // Exceptions whose stack was unwound by a catch leave their state behind; drop it.
        self.seh.retain(|s| s.ctx > esp0);
        if self.seh.len() >= 16 {
            return Err(unhandled); // exceptions inside exception handlers, all the way down
        }
        let ctx = esp0.wrapping_sub(CTX_SIZE + REC_SIZE + 16 + 8 + 16) & !0xF;
        let (rec, dispatcher, pointers) = (
            ctx + CTX_SIZE,
            ctx + CTX_SIZE + REC_SIZE,
            ctx + CTX_SIZE + REC_SIZE + 16,
        );
        if self
            .write_exception_records(ctx, rec, pointers, code, flags, address, params)
            .is_err()
        {
            return Err(unhandled); // no usable stack to dispatch on
        }
        let frame = self.mem.read(self.fs_base, 4).unwrap_or(END_OF_CHAIN);
        self.regs[ESP] = ctx;
        self.seh.push(SehState {
            ctx,
            pointers,
            rec,
            dispatcher,
            frame,
            noncontinuable: flags & EXCEPTION_NONCONTINUABLE != 0,
            code,
            address,
            unhandled,
        });
        self.seh_next()
    }

    #[allow(clippy::too_many_arguments)]
    fn write_exception_records(
        &mut self,
        ctx: u32,
        rec: u32,
        pointers: u32,
        code: u32,
        flags: u32,
        address: u32,
        params: &[u32],
    ) -> Result<(), Fault> {
        self.mem
            .map(ctx & !0xFFF, (CTX_SIZE + REC_SIZE + 64 + 0xFFF) & !0xFFF);
        self.mem
            .write_bytes(ctx, &vec![0u8; (CTX_SIZE + REC_SIZE + 24) as usize])?;
        self.mem.write(ctx, 4, CONTEXT_FULL)?;
        for (off, reg) in [
            (CTX_EDI, EDI),
            (CTX_ESI, ESI),
            (CTX_EBX, EBX),
            (CTX_EDX, EDX),
            (CTX_ECX, ECX),
            (CTX_EAX, EAX),
            (CTX_EBP, EBP),
        ] {
            self.mem.write(ctx + off, 4, self.regs[reg])?;
        }
        self.mem.write(ctx + CTX_EIP, 4, self.eip)?;
        self.mem.write(ctx + CTX_EFLAGS, 4, self.eflags)?;
        self.mem.write(ctx + CTX_ESP, 4, self.regs[ESP])?;
        self.mem.write(rec, 4, code)?;
        self.mem.write(rec + 4, 4, flags)?;
        self.mem.write(rec + 12, 4, address)?;
        let n = params.len().min(15) as u32;
        self.mem.write(rec + 16, 4, n)?;
        for (i, p) in params.iter().take(15).enumerate() {
            self.mem.write(rec + 20 + 4 * i as u32, 4, *p)?;
        }
        self.mem.write(pointers, 4, rec)?;
        self.mem.write(pointers + 4, 4, ctx)?;
        Ok(())
    }

    /// Call the handler of the current registration record, arranging for it to return to
    /// the dispatcher stub. With no records left, the exception is unhandled.
    fn seh_next(&mut self) -> Res<()> {
        loop {
            let Some(st) = self.seh.last() else {
                return Ok(());
            };
            let frame = st.frame;
            let readable = frame != END_OF_CHAIN
                && frame != 0
                && self.mem.is_mapped(frame)
                && self.mem.is_mapped(frame + 7);
            if !readable {
                return self.seh_unhandled();
            }
            let handler = self.mem.read(frame + 4, 4)?;
            if !self.mem.is_mapped(handler) {
                // A damaged record: skip it.
                let next = self.mem.read(frame, 4)?;
                self.seh.last_mut().unwrap().frame = next;
                continue;
            }
            let st = self.seh.last().unwrap();
            let (rec, ctx, dispatcher) = (st.rec, st.ctx, st.dispatcher);
            // cdecl: handler(record, frame, context, dispatcher); we pop the arguments on return.
            for arg in [dispatcher, ctx, frame, rec, self.seh_ret] {
                self.push(arg)?;
            }
            self.eip = handler;
            return Ok(());
        }
    }

    /// A handler returned to the dispatcher stub with its disposition in EAX.
    pub(crate) fn seh_returned(&mut self) -> Res<()> {
        let disposition = self.regs[EAX];
        self.regs[ESP] = self.regs[ESP].wrapping_add(16); // the handler's cdecl arguments
        let Some(st) = self.seh.last() else {
            return Err(Stop::Unimplemented(
                "returned to the exception dispatcher with no exception".into(),
            ));
        };
        if disposition == 0 && !st.noncontinuable {
            let ctx = st.ctx;
            self.seh.pop();
            return self.load_context(ctx);
        }
        // Continue search (or an unsupported disposition): on to the next registration record.
        let frame = st.frame;
        let next = self.mem.read(frame, 4)?;
        self.seh.last_mut().unwrap().frame = next;
        self.seh_next()
    }

    /// Nobody in the chain took it: give the process-wide filter a chance, then give up.
    fn seh_unhandled(&mut self) -> Res<()> {
        let st = self.seh.pop().expect("an exception is being dispatched");
        let filter = self.win.unhandled_filter;
        if filter != 0 {
            match self.call_function(filter, &[st.pointers])? {
                0xFFFF_FFFF => return self.load_context(st.ctx), // EXCEPTION_CONTINUE_EXECUTION
                1 => {
                    // EXCEPTION_EXECUTE_HANDLER: the program's crash handler ends the process.
                    let at = self
                        .win
                        .locate(st.address)
                        .unwrap_or_else(|| format!("{:#x}", st.address));
                    let bytes: Vec<String> = (0..8)
                        .filter_map(|i| self.mem.read_u8(st.address.wrapping_add(i)).ok())
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    eprintln!(
                        "[exception] {:#x} at {at} (bytes {}) ended the program via its unhandled-exception filter",
                        st.code,
                        bytes.join(" ")
                    );
                    return Err(Stop::Exit(st.code));
                }
                _ => {}
            }
        }
        Err(st.unhandled)
    }

    /// Resume the guest from a CONTEXT record (which a handler may have edited).
    fn load_context(&mut self, ctx: u32) -> Res<()> {
        for (off, reg) in [
            (CTX_EDI, EDI),
            (CTX_ESI, ESI),
            (CTX_EBX, EBX),
            (CTX_EDX, EDX),
            (CTX_ECX, ECX),
            (CTX_EAX, EAX),
            (CTX_EBP, EBP),
        ] {
            self.regs[reg] = self.mem.read(ctx + off, 4)?;
        }
        self.eip = self.mem.read(ctx + CTX_EIP, 4)?;
        self.eflags = self.mem.read(ctx + CTX_EFLAGS, 4)? | 2;
        self.regs[ESP] = self.mem.read(ctx + CTX_ESP, 4)?;
        Ok(())
    }
}

/// RaiseException(code, flags, nargs, args): behave as if the call had already returned, so
/// that if a handler resumes execution it continues right after the call.
pub fn raise_exception(e: &mut Emu, a: &Args) -> Res<u32> {
    let (code, flags, nargs, args) = (a[0], a[1], a[2].min(15), a[3]);
    let ret = e.pop()?;
    e.regs[ESP] = e.regs[ESP].wrapping_add(16);
    e.eip = ret;
    let mut params = Vec::new();
    if args != 0 {
        for i in 0..nargs {
            params.push(e.mem.read(args + 4 * i, 4)?);
        }
    }
    e.control_transferred = true;
    e.start_exception(
        code,
        flags,
        ret,
        &params,
        Stop::Exception { code, address: ret },
    )?;
    Ok(0)
}

/// RtlUnwind(targetFrame, targetIp, exceptionRecord, returnValue): run the termination
/// handlers of every registration record newer than `targetFrame`, unlinking each as it
/// goes, then return. (The caller, an exception handler, then jumps into its catch block.)
pub fn rtl_unwind(e: &mut Emu, a: &Args) -> Res<u32> {
    let (target, rec_in) = (a[0], a[2]);
    let esp0 = e.regs[ESP];
    // Scratch records on the stack, below everything live.
    let ctx = esp0.wrapping_sub(CTX_SIZE + REC_SIZE + 16 + 64) & !0xF;
    let (rec, dispatcher) = (ctx + CTX_SIZE, ctx + CTX_SIZE + REC_SIZE);
    e.mem
        .map(ctx & !0xFFF, (CTX_SIZE + REC_SIZE + 64 + 0xFFF) & !0xFFF);
    e.mem
        .write_bytes(ctx, &vec![0u8; (CTX_SIZE + REC_SIZE + 24) as usize])?;
    e.mem.write(ctx, 4, CONTEXT_FULL)?;
    if rec_in != 0 {
        // Mark the caller's record as an unwind in progress, as Windows does.
        let flags = e.mem.read(rec_in + 4, 4)?;
        e.mem.write(
            rec_in + 4,
            4,
            flags
                | EXCEPTION_UNWINDING
                | if target == END_OF_CHAIN {
                    EXCEPTION_EXIT_UNWIND
                } else {
                    0
                },
        )?;
    } else {
        e.mem.write(rec, 4, STATUS_UNWIND)?;
        e.mem.write(
            rec + 4,
            4,
            EXCEPTION_UNWINDING
                | if target == END_OF_CHAIN {
                    EXCEPTION_EXIT_UNWIND
                } else {
                    0
                },
        )?;
    }
    let record = if rec_in != 0 { rec_in } else { rec };
    let mut frame = e.mem.read(e.fs_base, 4)?;
    let mut guard = 0;
    while frame != END_OF_CHAIN && frame != 0 && frame != target && guard < 4096 {
        let handler = e.mem.read(frame + 4, 4)?;
        let next = e.mem.read(frame, 4)?;
        if e.mem.is_mapped(handler) {
            // handler(record, frame, context, dispatcher), in that argument order
            e.call_function(handler, &[record, frame, ctx, dispatcher])?;
        }
        e.mem.write(e.fs_base, 4, next)?; // unlink the frame we just unwound
        frame = next;
        guard += 1;
    }
    Ok(0)
}

#[cfg(test)]
mod tests;
