//! 32-bit x86 interpreter: the integer instruction set here, the x87 FPU in `fpu`, MMX in
//! `mmx` and SSE in `sse`.

pub mod fpu;
pub mod icache;
pub mod jit;
pub mod mmx;
pub mod sse;

use crate::cpu::fpu::Fpu;
use crate::memory::{Fault, Memory};
use crate::win32::{Win32, HLE_BASE, HLE_END};

pub const EAX: usize = 0;
pub const ECX: usize = 1;
pub const EDX: usize = 2;
pub const EBX: usize = 3;
pub const ESP: usize = 4;
pub const EBP: usize = 5;
pub const ESI: usize = 6;
pub const EDI: usize = 7;

/// Segment selector values of a 32-bit Windows XP process. The memory model is flat, so segment
/// registers never matter to addressing (FS is handled separately as the TEB pointer); programs
/// only ever save and restore them.
const SEG_CS: u32 = 0x1B;
const SEG_DATA: u32 = 0x23; // DS, ES, SS
const SEG_FS: u32 = 0x3B;

pub const CF: u32 = 1 << 0;
pub const PF: u32 = 1 << 2;
pub const ZF: u32 = 1 << 6;
pub const SF: u32 = 1 << 7;
pub const DF: u32 = 1 << 10;
pub const OF: u32 = 1 << 11;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    Exit(u32),
    Halt,
    StepLimit,
    /// The guest asked for a message and none is ready. State is intact: run again once
    /// input arrives and the same call is retried.
    WaitForInput,
    /// Every thread is waiting for a time to arrive; the host should sleep, then run again.
    Sleeping,
    /// Internal: the running thread cannot proceed (see `win32::threads`). `run` handles it.
    Blocked(crate::win32::threads::Block),
    /// Internal: the running thread has finished.
    ThreadExit,
    Fault(Fault),
    InvalidOpcode { eip: u32, bytes: [u8; 3] },
    DivideError(u32),
    Unimplemented(String),
    /// A software exception (RaiseException / C++ throw) that no handler took.
    Exception { code: u32, address: u32 },
}

impl From<Fault> for Stop {
    fn from(f: Fault) -> Stop {
        Stop::Fault(f)
    }
}

pub type Res<T> = Result<T, Stop>;

#[derive(Clone, Copy, Default)]
pub(crate) struct Prefix {
    pub opsize16: bool,
    pub fs: bool,
    /// 0, 0xF2 (repne) or 0xF3 (rep/repe)
    pub rep: u8,
}

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Reg(usize),
    Mem(u32),
}

#[derive(Clone, Copy)]
pub(crate) struct ModRm {
    pub reg: usize,
    pub rm: Op,
}

#[inline(always)]
fn mask(bits: u32) -> u32 {
    u32::MAX >> (32 - bits)
}

#[inline(always)]
fn sign_bit(bits: u32) -> u32 {
    1 << (bits - 1)
}

/// Bytes that are instruction prefixes (operand size, segment overrides, rep, lock).
const IS_PREFIX: [bool; 256] = {
    let mut t = [false; 256];
    let list = [0x66u8, 0x64, 0xF2, 0xF3, 0x26, 0x2E, 0x36, 0x3E, 0x65, 0x67, 0xF0];
    let mut i = 0;
    while i < list.len() {
        t[list[i] as usize] = true;
        i += 1;
    }
    t
};

/// PF for every byte value: set when the number of one bits is even.
const PARITY: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = (i as u32).count_ones().is_multiple_of(2) as u8;
        i += 1;
    }
    t
};

fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

/// `repr(C)` so that compiled code (`jit`) finds the registers at fixed offsets.
#[repr(C)]
pub struct Emu {
    pub regs: [u32; 8],
    pub eip: u32,
    pub eflags: u32,
    /// The memory page table and code-page map, for compiled code (set before it runs).
    pub(crate) jit_pages: *const usize,
    pub(crate) jit_code_pages: *const u8,
    pub jit: jit::Jit,
    /// Why a compiled block stopped, when it stopped with an error.
    pub(crate) jit_stop: Option<Stop>,
    /// Decoded instructions (see `icache`), used unless XP_NO_ICACHE is set.
    pub icache: icache::ICache,
    pub(crate) icache_on: bool,
    pub fs_base: u32,
    pub mem: Memory,
    pub win: Win32,
    pub fpu: Fpu,
    pub wm: crate::gui::wm::WindowManager,
    pub steps: u64,
    last_pump: u64,
    /// The last 16 instruction addresses executed, for crash reports.
    pub recent: [u32; 16],
    /// XP_PROFILE: sampled instruction addresses (every 1024th step), bucketed by 16 bytes.
    pub profile: Option<std::collections::HashMap<u32, u32>>,
    /// Host->guest calls in flight (`call_function`): the (eip, esp) of the API stub that
    /// made each call. Normally popped when the call returns; if a wait unwinds the host
    /// stack first they stay, and `finish_nested` completes the call when the guest returns.
    pub(crate) nested: Vec<(u32, u32)>,
    /// Exceptions being dispatched (see `win32::seh`), innermost last.
    pub(crate) seh: Vec<crate::win32::seh::SehState>,
    /// Address of the hidden stub exception handlers return to.
    pub(crate) seh_ret: u32,
    /// Set by an API handler that has already moved eip/esp itself (RaiseException).
    pub(crate) control_transferred: bool,
    // ---- threads (see `win32::threads`) ----
    pub(crate) threads: Vec<crate::win32::threads::Thread>,
    pub(crate) current: usize,
    /// Instructions the running thread has used of its time slice.
    pub(crate) slice: u32,
    pub quantum: u32,
    /// Consecutive blocked polls with no progress in between.
    pub(crate) stall: u32,
    /// When the last run ended with every thread waiting: the earliest deadline among them.
    pub(crate) wake_at: Option<u64>,
    pub(crate) pending_wait: Option<(u32, u32, u64)>,
}

impl Emu {
    pub fn new() -> Self {
        let mut win = Win32::new();
        let seh_ret = win.resolve("__emu", "seh_return");
        Emu {
            regs: [0; 8],
            eip: 0,
            eflags: 2,
            jit_pages: std::ptr::null(),
            jit_code_pages: std::ptr::null(),
            jit: jit::Jit::new(),
            jit_stop: None,
            icache: icache::ICache::new(),
            icache_on: std::env::var_os("XP_NO_ICACHE").is_none(),
            fs_base: 0,
            mem: Memory::new(),
            win,
            fpu: Fpu::new(),
            wm: crate::gui::wm::WindowManager::new(),
            steps: 0,
            last_pump: 0,
            recent: [0; 16],
            profile: None,
            nested: Vec::new(),
            seh: Vec::new(),
            seh_ret,
            control_transferred: false,
            threads: Vec::new(),
            current: 0,
            slice: 0,
            quantum: crate::win32::threads::DEFAULT_QUANTUM,
            stall: 0,
            wake_at: None,
            pending_wait: None,
        }
    }

    /// Call a guest stdcall function from the host and run it to completion.
    pub fn call_function(&mut self, addr: u32, args: &[u32]) -> Res<u32> {
        if self.regs[ESP] == 0 {
            crate::loader::init_process(self).map_err(Stop::Fault)?;
        }
        let sentinel = HLE_END - 4;
        let saved = (self.eip, self.regs[ESP]);
        for a in args.iter().rev() {
            self.push(*a)?;
        }
        self.push(sentinel)?;
        self.eip = addr;
        self.nested.push(saved);
        while self.eip != sentinel {
            self.step()?;
        }
        self.nested.pop();
        self.eip = saved.0;
        self.regs[ESP] = saved.1;
        Ok(self.regs[EAX])
    }

    /// The guest returned to the `call_function` sentinel after the host frame that made the
    /// call was abandoned (a wait unwound it). Finish what that API stub would have done:
    /// back to the stub, result already in EAX, then its normal stdcall/cdecl return.
    fn finish_nested(&mut self) -> Res<()> {
        let Some((eip, esp)) = self.nested.pop() else {
            return Err(Stop::Unimplemented("guest returned to a callback with no caller".into()));
        };
        self.eip = eip;
        self.regs[ESP] = esp;
        let idx = ((eip - HLE_BASE) / 4) as usize;
        let (argc, cdecl) = match self.win.stubs.get(idx) {
            Some(s) => (s.argc & 0xff, s.argc & crate::win32::CDECL != 0),
            None => return Err(Stop::Unimplemented(format!("callback resumed into unknown stub {eip:#x}"))),
        };
        self.eip = self.pop()?;
        if !cdecl {
            self.regs[ESP] = self.regs[ESP].wrapping_add(4 * argc as u32);
        }
        Ok(())
    }

    pub fn run(&mut self, max_steps: u64) -> Stop {
        // A new run gives every blocked thread a fresh chance to re-check its condition before
        // the scheduler concludes that nothing can proceed.
        self.stall = 0;
        let mut left = max_steps;
        while left > 0 {
            // Timers and audio buffers are serviced every couple of thousand instructions.
            if self.steps.wrapping_sub(self.last_pump) >= 0x800 {
                self.last_pump = self.steps;
                if self.win.audio.active()
                    && let Err(stop) = self.pump_devices()
                        && let Some(stop) = self.route_stop(stop) {
                            return stop;
                        }
            }
            // Run a batch of instructions; the bookkeeping above and below is per batch.
            let multi = self.threads.len() > 1;
            let mut n = left.min(256);
            if multi {
                n = n.min(u64::from(self.quantum.saturating_sub(self.slice)).max(1));
            }
            let mut failed = false;
            let mut done = 0;
            while done < n {
                let (k, r) = self.step_many(n - done);
                done += k;
                // Blocking and thread exits are the scheduler's business; the rest ends the run.
                if let Err(stop) = r {
                    failed = true;
                    if let Some(stop) = self.route_stop(stop) {
                        return stop;
                    }
                    break;
                }
            }
            left -= done;
            if !failed {
                self.stall = 0;
                if multi {
                    self.slice += done as u32;
                    if self.slice >= self.quantum {
                        self.slice = 0;
                        self.rotate();
                    }
                }
            }
        }
        Stop::StepLimit
    }

    // ---- fetch / stack ----------------------------------------------------

    #[inline(always)]
    pub(crate) fn fetch8(&mut self) -> Res<u8> {
        let v = self.mem.read_u8(self.eip)?;
        self.eip = self.eip.wrapping_add(1);
        Ok(v)
    }

    #[inline(always)]
    fn fetch_imm(&mut self, bits: u32) -> Res<u32> {
        let v = self.mem.read(self.eip, bits / 8)?;
        self.eip = self.eip.wrapping_add(bits / 8);
        Ok(v)
    }

    #[inline(always)]
    pub fn push(&mut self, v: u32) -> Res<()> {
        self.regs[ESP] = self.regs[ESP].wrapping_sub(4);
        self.mem.write(self.regs[ESP], 4, v)?;
        Ok(())
    }

    #[inline(always)]
    pub fn pop(&mut self) -> Res<u32> {
        let v = self.mem.read(self.regs[ESP], 4)?;
        self.regs[ESP] = self.regs[ESP].wrapping_add(4);
        Ok(v)
    }

    // ---- registers / operands --------------------------------------------

    #[inline(always)]
    fn get_reg(&self, r: usize, bits: u32) -> u32 {
        match bits {
            32 => self.regs[r],
            16 => self.regs[r] & 0xffff,
            _ if r < 4 => self.regs[r] & 0xff,
            _ => (self.regs[r - 4] >> 8) & 0xff,
        }
    }

    #[inline(always)]
    fn set_reg(&mut self, r: usize, bits: u32, v: u32) {
        match bits {
            32 => self.regs[r] = v,
            16 => self.regs[r] = (self.regs[r] & 0xffff_0000) | (v & 0xffff),
            _ if r < 4 => self.regs[r] = (self.regs[r] & 0xffff_ff00) | (v & 0xff),
            _ => self.regs[r - 4] = (self.regs[r - 4] & 0xffff_00ff) | ((v & 0xff) << 8),
        }
    }

    #[inline(always)]
    pub(crate) fn read_op(&self, op: Op, bits: u32) -> Res<u32> {
        match op {
            Op::Reg(r) => Ok(self.get_reg(r, bits)),
            Op::Mem(a) => Ok(self.mem.read(a, bits / 8)?),
        }
    }

    #[inline(always)]
    pub(crate) fn write_op(&mut self, op: Op, bits: u32, v: u32) -> Res<()> {
        match op {
            Op::Reg(r) => {
                self.set_reg(r, bits, v);
                Ok(())
            }
            Op::Mem(a) => Ok(self.mem.write(a, bits / 8, v)?),
        }
    }

    #[inline(always)]
    pub(crate) fn modrm(&mut self, p: &Prefix) -> Res<ModRm> {
        let b = self.fetch8()?;
        let md = b >> 6;
        let reg = ((b >> 3) & 7) as usize;
        let rm = (b & 7) as usize;
        if md == 3 {
            return Ok(ModRm { reg, rm: Op::Reg(rm) });
        }
        let mut addr;
        if rm == 4 {
            let sib = self.fetch8()?;
            let scale = sib >> 6;
            let idx = ((sib >> 3) & 7) as usize;
            let base = (sib & 7) as usize;
            addr = if base == 5 && md == 0 {
                self.fetch_imm(32)?
            } else {
                self.regs[base]
            };
            if idx != 4 {
                addr = addr.wrapping_add(self.regs[idx] << scale);
            }
        } else if rm == 5 && md == 0 {
            addr = self.fetch_imm(32)?;
        } else {
            addr = self.regs[rm];
        }
        match md {
            1 => addr = addr.wrapping_add(self.fetch8()? as i8 as u32),
            2 => addr = addr.wrapping_add(self.fetch_imm(32)?),
            _ => {}
        }
        if p.fs {
            addr = addr.wrapping_add(self.fs_base);
        }
        Ok(ModRm { reg, rm: Op::Mem(addr) })
    }

    // ---- flags / ALU -------------------------------------------------------

    #[inline(always)]
    fn flag(&self, f: u32) -> bool {
        self.eflags & f != 0
    }

    #[inline(always)]
    pub(crate) fn set_flag(&mut self, f: u32, on: bool) {
        if on {
            self.eflags |= f;
        } else {
            self.eflags &= !f;
        }
    }

    #[inline(always)]
    fn set_szp(&mut self, r: u32, bits: u32) {
        let f = (((r == 0) as u32) * ZF) | (((r & sign_bit(bits) != 0) as u32) * SF) | ((PARITY[(r & 0xFF) as usize] as u32) * PF);
        self.eflags = (self.eflags & !(ZF | SF | PF)) | f;
    }

    /// Set all of CF, OF, ZF, SF and PF at once (the arithmetic and logic instructions).
    #[inline(always)]
    fn set_arith_flags(&mut self, r: u32, bits: u32, cf: bool, of: bool) {
        let f = ((cf as u32) * CF) | ((of as u32) * OF) | (((r == 0) as u32) * ZF) | (((r & sign_bit(bits) != 0) as u32) * SF) | ((PARITY[(r & 0xFF) as usize] as u32) * PF);
        self.eflags = (self.eflags & !(CF | OF | ZF | SF | PF)) | f;
    }

    /// op: 0 add, 1 or, 2 adc, 3 sbb, 4 and, 5 sub, 6 xor, 7 cmp.
    #[inline(always)]
    fn alu(&mut self, op: usize, a: u32, b: u32, bits: u32) -> u32 {
        let m = mask(bits);
        let s = sign_bit(bits);
        let (r, cf, of) = match op {
            0 | 2 => {
                let carry_in = (self.eflags & CF) * (op == 2) as u32; // CF is bit 0
                let wide = a as u64 + b as u64 + carry_in as u64;
                let r = (wide as u32) & m;
                (r, wide > m as u64, (a ^ r) & (b ^ r) & s != 0)
            }
            3 | 5 | 7 => {
                let carry_in = (self.eflags & CF) * (op == 3) as u32;
                let r = a.wrapping_sub(b).wrapping_sub(carry_in) & m;
                (r, (a as u64) < b as u64 + carry_in as u64, (a ^ b) & (a ^ r) & s != 0)
            }
            1 => ((a | b) & m, false, false),
            4 => ((a & b) & m, false, false),
            _ => ((a ^ b) & m, false, false),
        };
        self.set_arith_flags(r, bits, cf, of);
        r
    }

    /// op: 0 rol, 1 ror, 4/6 shl, 5 shr, 7 sar.
    fn shift(&mut self, op: usize, a: u32, count: u32, bits: u32) -> Res<u32> {
        let count = count & 31;
        if count == 0 {
            return Ok(a);
        }
        let m = mask(bits);
        let msb = |v: u32| v & sign_bit(bits) != 0;
        match op {
            0 => {
                let c = count % bits;
                let r = if c == 0 { a } else { ((a << c) | (a >> (bits - c))) & m };
                self.set_flag(CF, r & 1 != 0);
                Ok(r)
            }
            1 => {
                let c = count % bits;
                let r = if c == 0 { a } else { ((a >> c) | (a << (bits - c))) & m };
                self.set_flag(CF, msb(r));
                Ok(r)
            }
            4 | 6 => {
                let wide = (a as u64) << count;
                let r = (wide as u32) & m;
                self.set_flag(CF, (wide >> bits) & 1 != 0);
                self.set_flag(OF, msb(r) != self.flag(CF));
                self.set_szp(r, bits);
                Ok(r)
            }
            5 => {
                let r = a >> count;
                self.set_flag(CF, (a >> (count - 1)) & 1 != 0);
                self.set_flag(OF, msb(a));
                self.set_szp(r, bits);
                Ok(r)
            }
            7 => {
                let sa = sext(a, bits);
                let r = ((sa >> count) as u32) & m;
                self.set_flag(CF, ((sa >> (count - 1)) & 1) != 0);
                self.set_flag(OF, false);
                self.set_szp(r, bits);
                Ok(r)
            }
            _ => Err(Stop::Unimplemented("rcl/rcr".into())),
        }
    }

    #[inline(always)]
    fn cond(&self, cc: u8) -> bool {
        let r = match cc >> 1 {
            0 => self.flag(OF),
            1 => self.flag(CF),
            2 => self.flag(ZF),
            3 => self.flag(CF) || self.flag(ZF),
            4 => self.flag(SF),
            5 => self.flag(PF),
            6 => self.flag(SF) != self.flag(OF),
            _ => self.flag(ZF) || (self.flag(SF) != self.flag(OF)),
        };
        r != (cc & 1 != 0)
    }

    /// (low, high) halves of the accumulator pair used by mul/div for this width.
    fn acc_pair(&self, bits: u32) -> (u32, u32) {
        match bits {
            32 => (self.regs[EAX], self.regs[EDX]),
            16 => (self.regs[EAX] & 0xffff, self.regs[EDX] & 0xffff),
            _ => (self.regs[EAX] & 0xff, (self.regs[EAX] >> 8) & 0xff),
        }
    }

    fn set_acc_pair(&mut self, bits: u32, lo: u32, hi: u32) {
        match bits {
            32 => {
                self.regs[EAX] = lo;
                self.regs[EDX] = hi;
            }
            16 => {
                self.set_reg(EAX, 16, lo);
                self.set_reg(EDX, 16, hi);
            }
            _ => self.set_reg(EAX, 16, (lo & 0xff) | ((hi & 0xff) << 8)),
        }
    }

    // ---- execution ---------------------------------------------------------

    /// Run at most `limit` instructions: a whole cached block when there is one, otherwise a
    /// single `step`. Returns how many ran and how the last one went.
    #[inline(always)]
    fn step_many(&mut self, limit: u64) -> (u64, Res<()>) {
        // Debugging aids watch every instruction, so they take the single-step path.
        if self.icache_on && self.profile.is_none() && self.mem.watch.is_none() {
            if let Some(r) = self.run_block(limit) {
                return r;
            }
        }
        (1, self.step())
    }

    #[inline(always)]
    pub fn step(&mut self) -> Res<()> {
        self.steps += 1;
        let start = self.eip;
        // Stubs (HLE) and the SEH return trampoline live outside normal code.
        if start.wrapping_sub(HLE_BASE) < HLE_END - HLE_BASE || start == self.seh_ret {
            return self.step_special();
        }
        self.recent[(self.steps & 15) as usize] = start;
        if let Some(p) = self.profile.as_mut() {
            if self.steps & 1023 == 0 {
                *p.entry(start & !15).or_insert(0) += 1;
            }
            // opcode histogram, under keys with the top bit set
            if let Ok(b) = self.mem.read_u8(start) {
                let key = if b == 0x0F { 0x8000_0000 | 0x100 | self.mem.read_u8(start + 1).unwrap_or(0) as u32 } else { 0x8000_0000 | b as u32 };
                *p.entry(key).or_insert(0) += 1;
            }
        }
        let saved = (self.regs, self.eflags);
        let r = if self.icache_on { self.step_cached(start) } else { self.step_decoded(start) };
        if self.mem.watch.is_some() {
            for (addr, n, val) in self.mem.take_watch_hits() {
                let loc = self.win.locate(start).unwrap_or_else(|| format!("{start:#x}"));
                eprintln!("[watch] {n}-byte write of {val:#x} to {addr:#x} by {loc}");
            }
        }
        match r {
            // A hardware fault is a Windows exception: give the guest's handlers a go first.
            Err(stop @ (Stop::Fault(_) | Stop::DivideError(_) | Stop::InvalidOpcode { .. })) => self.raise_hardware(stop, start, saved),
            other => other,
        }
    }

    #[cold]
    #[inline(never)]
    fn step_special(&mut self) -> Res<()> {
        if self.eip == HLE_END - 4 {
            return self.finish_nested();
        }
        if self.eip == self.seh_ret {
            return self.seh_returned();
        }
        self.call_hle()
    }

    #[inline(always)]
    fn step_decoded(&mut self, start: u32) -> Res<()> {
        let mut p = Prefix::default();
        let first = self.fetch8()?;
        if !IS_PREFIX[first as usize] {
            return self.exec(first, p, start);
        }
        let mut pending = Some(first);
        let opc = loop {
            let b = match pending.take() {
                Some(b) => b,
                None => self.fetch8()?,
            };
            match b {
                0x66 => p.opsize16 = true,
                0x64 => p.fs = true,
                0xF2 | 0xF3 => p.rep = b,
                // segment overrides we do not model, address-size, and `lock` (one thread)
                0x26 | 0x2E | 0x36 | 0x3E | 0x65 | 0x67 | 0xF0 => {}
                _ => break b,
            }
        };
        self.exec(opc, p, start)
    }

    pub(crate) fn invalid(&self, start: u32) -> Stop {
        let mut bytes = [0u8; 3];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = self.mem.read_u8(start.wrapping_add(i as u32)).unwrap_or(0);
        }
        Stop::InvalidOpcode { eip: start, bytes }
    }

    fn exec(&mut self, opc: u8, p: Prefix, start: u32) -> Res<()> {
        let bits = if p.opsize16 { 16 } else { 32 };
        // add/or/adc/sbb/and/sub/xor/cmp in their six encodings
        if opc < 0x40 && (opc & 7) < 6 {
            let op = (opc >> 3) as usize;
            match opc & 7 {
                k @ 0..=3 => {
                    let b = if k & 1 == 0 { 8 } else { bits };
                    let m = self.modrm(&p)?;
                    let rm_v = self.read_op(m.rm, b)?;
                    let r_v = self.get_reg(m.reg, b);
                    if k < 2 {
                        let r = self.alu(op, rm_v, r_v, b);
                        if op != 7 {
                            self.write_op(m.rm, b, r)?;
                        }
                    } else {
                        let r = self.alu(op, r_v, rm_v, b);
                        if op != 7 {
                            self.set_reg(m.reg, b, r);
                        }
                    }
                }
                k => {
                    let b = if k == 4 { 8 } else { bits };
                    let imm = self.fetch_imm(b)?;
                    let a = self.get_reg(EAX, b);
                    let r = self.alu(op, a, imm, b);
                    if op != 7 {
                        self.set_reg(EAX, b, r);
                    }
                }
            }
            return Ok(());
        }
        match opc {
            0x0F => return self.exec0f(p, start),
            0x40..=0x4F => {
                let r = (opc & 7) as usize;
                let a = self.get_reg(r, bits);
                let cf = self.flag(CF);
                let res = self.alu(if opc < 0x48 { 0 } else { 5 }, a, 1, bits);
                self.set_flag(CF, cf);
                self.set_reg(r, bits, res);
            }
            0x50..=0x57 => self.push(self.regs[(opc & 7) as usize])?,
            0x58..=0x5F => {
                let v = self.pop()?;
                self.regs[(opc & 7) as usize] = v;
            }
            0x60 => {
                let esp = self.regs[ESP];
                for r in 0..8 {
                    self.push(if r == ESP { esp } else { self.regs[r] })?;
                }
            }
            0x61 => {
                for r in (0..8).rev() {
                    let v = self.pop()?;
                    if r != ESP {
                        self.regs[r] = v;
                    }
                }
            }
            0x68 => {
                let v = self.fetch_imm(32)?;
                self.push(v)?;
            }
            0x6A => {
                let v = self.fetch8()? as i8 as u32;
                self.push(v)?;
            }
            0x69 | 0x6B => {
                let m = self.modrm(&p)?;
                let a = self.read_op(m.rm, bits)?;
                let imm = if opc == 0x69 {
                    self.fetch_imm(bits)?
                } else {
                    self.fetch8()? as i8 as u32
                };
                let r = self.imul2(a, imm, bits);
                self.set_reg(m.reg, bits, r);
            }
            0x70..=0x7F => {
                let rel = self.fetch8()? as i8 as u32;
                if self.cond(opc & 15) {
                    self.eip = self.eip.wrapping_add(rel);
                }
            }
            0x80 | 0x81 | 0x83 => {
                let b = if opc == 0x80 { 8 } else { bits };
                let m = self.modrm(&p)?;
                let imm = match opc {
                    0x80 => self.fetch8()? as u32,
                    0x81 => self.fetch_imm(bits)?,
                    _ => (self.fetch8()? as i8 as u32) & mask(b),
                };
                let a = self.read_op(m.rm, b)?;
                let r = self.alu(m.reg, a, imm, b);
                if m.reg != 7 {
                    self.write_op(m.rm, b, r)?;
                }
            }
            0x84 | 0x85 => {
                let b = if opc == 0x84 { 8 } else { bits };
                let m = self.modrm(&p)?;
                let a = self.read_op(m.rm, b)?;
                let c = self.get_reg(m.reg, b);
                self.alu(4, a, c, b);
            }
            0x86 | 0x87 => {
                let b = if opc == 0x86 { 8 } else { bits };
                let m = self.modrm(&p)?;
                let a = self.read_op(m.rm, b)?;
                let c = self.get_reg(m.reg, b);
                self.write_op(m.rm, b, c)?;
                self.set_reg(m.reg, b, a);
            }
            0x89 | 0x8B if bits == 32 => {
                let m = self.modrm(&p)?;
                if opc == 0x89 {
                    let v = self.regs[m.reg];
                    match m.rm {
                        Op::Reg(r) => self.regs[r] = v,
                        Op::Mem(a) => self.mem.write(a, 4, v)?,
                    }
                } else {
                    self.regs[m.reg] = match m.rm {
                        Op::Reg(r) => self.regs[r],
                        Op::Mem(a) => self.mem.read(a, 4)?,
                    };
                }
            }
            0x88..=0x8B => {
                let b = if opc & 1 == 0 { 8 } else { bits };
                let m = self.modrm(&p)?;
                if opc < 0x8A {
                    let v = self.get_reg(m.reg, b);
                    self.write_op(m.rm, b, v)?;
                } else {
                    let v = self.read_op(m.rm, b)?;
                    self.set_reg(m.reg, b, v);
                }
            }
            0x8D => {
                let m = self.modrm(&Prefix::default())?;
                match m.rm {
                    Op::Mem(a) => self.set_reg(m.reg, bits, a),
                    Op::Reg(_) => return Err(self.invalid(start)),
                }
            }
            0x8F => {
                let m = self.modrm(&p)?;
                let v = self.pop()?;
                self.write_op(m.rm, 32, v)?;
            }
            0xD7 => {
                // xlat: al = [ebx + al]
                let mut addr = self.regs[EBX].wrapping_add(self.regs[EAX] & 0xFF);
                if p.fs {
                    addr = addr.wrapping_add(self.fs_base);
                }
                let v = self.mem.read(addr, 1)?;
                self.set_reg(EAX, 8, v);
            }
            0x90 | 0x9B => {} // nop; fwait (no pending x87 exceptions to wait for)
            // push/pop of segment registers: values are fixed, pops are discarded
            0x06 | 0x16 | 0x1E => self.push(SEG_DATA)?,
            0x0E => self.push(SEG_CS)?,
            0x07 | 0x17 | 0x1F => {
                self.pop()?;
            }
            // mov r/m, Sreg   /   mov Sreg, r/m
            0x8C => {
                let m = self.modrm(&p)?;
                let sel = match m.reg {
                    1 => SEG_CS,
                    4 => SEG_FS,
                    5 => 0,
                    _ => SEG_DATA,
                };
                match m.rm {
                    Op::Reg(r) if !p.opsize16 => self.regs[r] = sel,
                    rm => self.write_op(rm, 16, sel)?,
                }
            }
            0x8E => {
                let m = self.modrm(&p)?;
                self.read_op(m.rm, 16)?; // the selector is read (and may fault) but has no effect
            }
            0x91..=0x97 => {
                let r = (opc & 7) as usize;
                let a = self.get_reg(EAX, bits);
                let b = self.get_reg(r, bits);
                self.set_reg(EAX, bits, b);
                self.set_reg(r, bits, a);
            }
            0x98 => {
                if p.opsize16 {
                    let v = sext(self.regs[EAX], 8) as u32;
                    self.set_reg(EAX, 16, v);
                } else {
                    self.regs[EAX] = sext(self.regs[EAX], 16) as u32;
                }
            }
            0x99 => {
                if p.opsize16 {
                    let neg = self.regs[EAX] & 0x8000 != 0;
                    self.set_reg(EDX, 16, if neg { 0xffff } else { 0 });
                } else {
                    self.regs[EDX] = if self.regs[EAX] & 0x8000_0000 != 0 { u32::MAX } else { 0 };
                }
            }
            0x9E => {
                let ah = (self.regs[EAX] >> 8) & 0xff;
                let m = CF | PF | ZF | SF | 0x10;
                self.eflags = (self.eflags & !m) | (ah & m) | 2;
            }
            0x9F => {
                let f = (self.eflags & (CF | PF | ZF | SF | 0x10)) | 2;
                self.set_reg(4, 8, f); // AH
            }
            0xD8..=0xDF if opc != 0xD0 && opc != 0xD1 && opc != 0xD2 && opc != 0xD3 => {
                return self.exec_fpu(opc, p, start);
            }
            0x9C => self.push(self.eflags | 2)?,
            0x9D => self.eflags = self.pop()? | 2,
            0xA0..=0xA3 => {
                let mut addr = self.fetch_imm(32)?;
                if p.fs {
                    addr = addr.wrapping_add(self.fs_base);
                }
                let b = if opc & 1 == 0 { 8 } else { bits };
                if opc < 0xA2 {
                    let v = self.mem.read(addr, b / 8)?;
                    self.set_reg(EAX, b, v);
                } else {
                    let v = self.get_reg(EAX, b);
                    self.mem.write(addr, b / 8, v)?;
                }
            }
            0xA4..=0xA7 | 0xAA..=0xAF => self.string_op(opc, &p)?,
            0xA8 | 0xA9 => {
                let b = if opc == 0xA8 { 8 } else { bits };
                let imm = self.fetch_imm(b)?;
                let a = self.get_reg(EAX, b);
                self.alu(4, a, imm, b);
            }
            0xB0..=0xB7 => {
                let v = self.fetch8()? as u32;
                self.set_reg((opc & 7) as usize, 8, v);
            }
            0xB8..=0xBF => {
                let v = self.fetch_imm(bits)?;
                self.set_reg((opc & 7) as usize, bits, v);
            }
            0xC0 | 0xC1 | 0xD0..=0xD3 => {
                let b = if opc & 1 == 0 { 8 } else { bits };
                let m = self.modrm(&p)?;
                let count = match opc {
                    0xC0 | 0xC1 => self.fetch8()? as u32,
                    0xD0 | 0xD1 => 1,
                    _ => self.regs[ECX] & 0xff,
                };
                let v = self.read_op(m.rm, b)?;
                let r = self.shift(m.reg, v, count, b)?;
                self.write_op(m.rm, b, r)?;
            }
            0xC2 => {
                let n = self.fetch_imm(16)?;
                self.eip = self.pop()?;
                self.regs[ESP] = self.regs[ESP].wrapping_add(n);
            }
            0xC3 => self.eip = self.pop()?,
            0xC6 | 0xC7 => {
                let b = if opc == 0xC6 { 8 } else { bits };
                let m = self.modrm(&p)?;
                let imm = self.fetch_imm(b)?;
                self.write_op(m.rm, b, imm)?;
            }
            0xC9 => {
                self.regs[ESP] = self.regs[EBP];
                self.regs[EBP] = self.pop()?;
            }
            0xE2 => {
                let rel = self.fetch8()? as i8 as u32;
                self.regs[ECX] = self.regs[ECX].wrapping_sub(1);
                if self.regs[ECX] != 0 {
                    self.eip = self.eip.wrapping_add(rel);
                }
            }
            0xE3 => {
                let rel = self.fetch8()? as i8 as u32;
                if self.regs[ECX] == 0 {
                    self.eip = self.eip.wrapping_add(rel);
                }
            }
            0xE8 => {
                let rel = self.fetch_imm(32)?;
                self.push(self.eip)?;
                self.eip = self.eip.wrapping_add(rel);
            }
            0xE9 => {
                let rel = self.fetch_imm(32)?;
                self.eip = self.eip.wrapping_add(rel);
            }
            0xEB => {
                let rel = self.fetch8()? as i8 as u32;
                self.eip = self.eip.wrapping_add(rel);
            }
            0xF4 => return Err(Stop::Halt),
            0xF5 => self.eflags ^= CF,
            0xF8 => self.set_flag(CF, false),
            0xF9 => self.set_flag(CF, true),
            0xFC => self.set_flag(DF, false),
            0xFD => self.set_flag(DF, true),
            0xF6 | 0xF7 => self.group3(opc, &p, start)?,
            0xFE | 0xFF => {
                let b = if opc == 0xFE { 8 } else { bits };
                let m = self.modrm(&p)?;
                match m.reg {
                    0 | 1 => {
                        let a = self.read_op(m.rm, b)?;
                        let cf = self.flag(CF);
                        let r = self.alu(if m.reg == 0 { 0 } else { 5 }, a, 1, b);
                        self.set_flag(CF, cf);
                        self.write_op(m.rm, b, r)?;
                    }
                    2 if opc == 0xFF => {
                        let target = self.read_op(m.rm, 32)?;
                        self.push(self.eip)?;
                        self.eip = target;
                    }
                    4 if opc == 0xFF => self.eip = self.read_op(m.rm, 32)?,
                    6 if opc == 0xFF => {
                        let v = self.read_op(m.rm, 32)?;
                        self.push(v)?;
                    }
                    _ => return Err(self.invalid(start)),
                }
            }
            _ => return Err(self.invalid(start)),
        }
        Ok(())
    }

    /// bt/bts/btr/btc: copy the bit into CF, then (kind 1/2/3) set/reset/complement it.
    /// With a register bit offset and a memory operand the offset may reach past the operand.
    fn bit_test(&mut self, rm: Op, offset: u32, bits: u32, kind: u8, reg_offset: bool) -> Res<()> {
        let (op, bit) = match rm {
            Op::Mem(addr) if reg_offset => {
                let signed = sext(offset, bits) as i64;
                let unit = (bits / 8) as i64;
                let word = signed.div_euclid(bits as i64);
                (Op::Mem(addr.wrapping_add((word * unit) as u32)), signed.rem_euclid(bits as i64) as u32)
            }
            other => (other, offset & (bits - 1)),
        };
        let v = self.read_op(op, bits)?;
        self.set_flag(CF, (v >> bit) & 1 != 0);
        let new = match kind {
            1 => v | (1 << bit),
            2 => v & !(1 << bit),
            3 => v ^ (1 << bit),
            _ => return Ok(()),
        };
        self.write_op(op, bits, new)
    }

    fn imul2(&mut self, a: u32, b: u32, bits: u32) -> u32 {
        let wide = sext(a, bits) as i64 * sext(b, bits) as i64;
        let r = (wide as u32) & mask(bits);
        let overflow = wide != sext(r, bits) as i64;
        self.set_flag(CF, overflow);
        self.set_flag(OF, overflow);
        r
    }

    fn group3(&mut self, opc: u8, p: &Prefix, start: u32) -> Res<()> {
        let bits = if opc == 0xF6 { 8 } else if p.opsize16 { 16 } else { 32 };
        let m = self.modrm(p)?;
        let v = self.read_op(m.rm, bits)?;
        let msk = mask(bits);
        match m.reg {
            0 | 1 => {
                let imm = self.fetch_imm(bits)?;
                self.alu(4, v, imm, bits);
            }
            2 => self.write_op(m.rm, bits, !v & msk)?,
            3 => {
                let r = self.alu(5, 0, v, bits);
                self.set_flag(CF, v != 0);
                self.write_op(m.rm, bits, r)?;
            }
            4 => {
                let (lo, _) = self.acc_pair(bits);
                let prod = lo as u64 * v as u64;
                self.set_acc_pair(bits, prod as u32 & msk, (prod >> bits) as u32 & msk);
                let hi = (prod >> bits) != 0;
                self.set_flag(CF, hi);
                self.set_flag(OF, hi);
            }
            5 => {
                let (lo, _) = self.acc_pair(bits);
                let prod = sext(lo, bits) as i64 * sext(v, bits) as i64;
                let lo_r = prod as u32 & msk;
                self.set_acc_pair(bits, lo_r, (prod >> bits) as u32 & msk);
                let overflow = prod != sext(lo_r, bits) as i64;
                self.set_flag(CF, overflow);
                self.set_flag(OF, overflow);
            }
            6 => {
                let (lo, hi) = self.acc_pair(bits);
                let dividend = ((hi as u64) << bits) | lo as u64;
                if v == 0 || dividend / v as u64 > msk as u64 {
                    return Err(Stop::DivideError(start));
                }
                self.set_acc_pair(bits, (dividend / v as u64) as u32, (dividend % v as u64) as u32);
            }
            _ => {
                let (lo, hi) = self.acc_pair(bits);
                let dividend = (((hi as u64) << bits) | lo as u64) as i64;
                let dividend = (dividend << (64 - 2 * bits)) >> (64 - 2 * bits);
                let d = sext(v, bits) as i64;
                if d == 0 {
                    return Err(Stop::DivideError(start));
                }
                let q = dividend / d;
                if q != sext(q as u32 & msk, bits) as i64 {
                    return Err(Stop::DivideError(start));
                }
                self.set_acc_pair(bits, q as u32 & msk, (dividend % d) as u32 & msk);
            }
        }
        Ok(())
    }

    fn string_op(&mut self, opc: u8, p: &Prefix) -> Res<()> {
        let bits = if opc & 1 == 0 { 8 } else if p.opsize16 { 16 } else { 32 };
        let n = bits / 8;
        let delta = if self.flag(DF) { (n as i32).wrapping_neg() } else { n as i32 } as u32;
        let compares = matches!(opc & 0xFE, 0xA6 | 0xAE);
        loop {
            if p.rep != 0 && self.regs[ECX] == 0 {
                break;
            }
            let (esi, edi) = (self.regs[ESI], self.regs[EDI]);
            match opc & 0xFE {
                0xA4 => {
                    let v = self.mem.read(esi, n)?;
                    self.mem.write(edi, n, v)?;
                    self.regs[ESI] = esi.wrapping_add(delta);
                    self.regs[EDI] = edi.wrapping_add(delta);
                }
                0xA6 => {
                    let (a, b) = (self.mem.read(esi, n)?, self.mem.read(edi, n)?);
                    self.alu(7, a, b, bits);
                    self.regs[ESI] = esi.wrapping_add(delta);
                    self.regs[EDI] = edi.wrapping_add(delta);
                }
                0xAA => {
                    self.mem.write(edi, n, self.get_reg(EAX, bits))?;
                    self.regs[EDI] = edi.wrapping_add(delta);
                }
                0xAC => {
                    let v = self.mem.read(esi, n)?;
                    self.set_reg(EAX, bits, v);
                    self.regs[ESI] = esi.wrapping_add(delta);
                }
                _ => {
                    let a = self.get_reg(EAX, bits);
                    let b = self.mem.read(edi, n)?;
                    self.alu(7, a, b, bits);
                    self.regs[EDI] = edi.wrapping_add(delta);
                }
            }
            if p.rep == 0 {
                break;
            }
            self.regs[ECX] = self.regs[ECX].wrapping_sub(1);
            if compares {
                let zf = self.flag(ZF);
                if (p.rep == 0xF3 && !zf) || (p.rep == 0xF2 && zf) {
                    break;
                }
            }
        }
        Ok(())
    }

    fn exec0f(&mut self, p: Prefix, start: u32) -> Res<()> {
        let bits = if p.opsize16 { 16 } else { 32 };
        let opc = self.fetch8()?;
        if sse::is_sse(opc) {
            if self.exec_sse(opc, &p)? {
                return Ok(());
            }
            return Err(self.invalid(start));
        }
        // MMX opcodes occupy 0x60..=0x7F and 0xD0..=0xFF, which no other 0F instruction uses here.
        if (0x60..=0x7F).contains(&opc) || opc >= 0xD0 {
            if self.exec_mmx(opc, &p)? {
                return Ok(());
            }
            return Err(self.invalid(start));
        }
        match opc {
            0x40..=0x4F => {
                let m = self.modrm(&p)?;
                let v = self.read_op(m.rm, bits)?;
                if self.cond(opc & 15) {
                    self.set_reg(m.reg, bits, v);
                }
            }
            0x80..=0x8F => {
                let rel = self.fetch_imm(32)?;
                if self.cond(opc & 15) {
                    self.eip = self.eip.wrapping_add(rel);
                }
            }
            0x90..=0x9F => {
                let m = self.modrm(&p)?;
                let v = self.cond(opc & 15) as u32;
                self.write_op(m.rm, 8, v)?;
            }
            0xA2 => {
                // cpuid: a "GenuineIntel" Pentium III: FPU, TSC, CMPXCHG8B, MMX and SSE; no SSE2.
                match self.regs[EAX] {
                    0 => {
                        self.regs[EAX] = 1;
                        self.regs[EBX] = u32::from_le_bytes(*b"Genu");
                        self.regs[EDX] = u32::from_le_bytes(*b"ineI");
                        self.regs[ECX] = u32::from_le_bytes(*b"ntel");
                    }
                    1 => {
                        self.regs[EAX] = 0x0000_0673; // family 6, model 7 (Katmai), stepping 3
                        self.regs[EBX] = 0;
                        self.regs[ECX] = 0;
                        // XP_NO_SSE=1 reports a Pentium II instead (MMX only), for comparison.
                        let sse = if std::env::var_os("XP_NO_SSE").is_some() { 0 } else { 1 << 25 };
                        self.regs[EDX] = 1 | 1 << 4 | 1 << 8 | 1 << 23 | sse;
                    }
                    _ => {
                        self.regs[EAX] = 0;
                        self.regs[EBX] = 0;
                        self.regs[ECX] = 0;
                        self.regs[EDX] = 0;
                    }
                }
            }
            0x31 => {
                self.regs[EAX] = self.steps as u32;
                self.regs[EDX] = (self.steps >> 32) as u32;
            }
            0xAF => {
                let m = self.modrm(&p)?;
                let a = self.get_reg(m.reg, bits);
                let b = self.read_op(m.rm, bits)?;
                let r = self.imul2(a, b, bits);
                self.set_reg(m.reg, bits, r);
            }
            0xB6 | 0xB7 | 0xBE | 0xBF => {
                let src_bits = if opc & 1 == 0 { 8 } else { 16 };
                let m = self.modrm(&p)?;
                let v = self.read_op(m.rm, src_bits)?;
                let v = if opc >= 0xBE { sext(v, src_bits) as u32 } else { v };
                self.set_reg(m.reg, bits, v);
            }
            0xC8..=0xCF => {
                let r = (opc & 7) as usize;
                self.regs[r] = self.regs[r].swap_bytes();
            }
            0xA0 => self.push(SEG_FS)?,  // push fs
            0xA8 => self.push(0)?,       // push gs
            0xA1 | 0xA9 => {             // pop fs / pop gs: ignored (FS stays the TEB pointer)
                self.pop()?;
            }
            // multi-byte nop / prefetch hints: decode the operand and do nothing
            0x18 | 0x19 | 0x1A | 0x1B | 0x1C | 0x1D | 0x1E | 0x1F | 0x0D => {
                self.modrm(&p)?;
            }
            // bt / bts / btr / btc  r/m, r
            0xA3 | 0xAB | 0xB3 | 0xBB => {
                let m = self.modrm(&p)?;
                let bit = self.get_reg(m.reg, bits);
                self.bit_test(m.rm, bit, bits, (opc >> 3) & 3, true)?;
            }
            // group 8: bt/bts/btr/btc  r/m, imm8
            0xBA => {
                let m = self.modrm(&p)?;
                let imm = self.fetch8()? as u32;
                if m.reg < 4 {
                    return Err(self.invalid(start));
                }
                self.bit_test(m.rm, imm, bits, (m.reg - 4) as u8, false)?;
            }
            // shld / shrd  r/m, r, imm8|cl
            0xA4 | 0xA5 | 0xAC | 0xAD => {
                let m = self.modrm(&p)?;
                let count = if opc & 1 == 0 { self.fetch8()? as u32 } else { self.regs[ECX] } & 31;
                if count != 0 {
                    let dst = self.read_op(m.rm, bits)?;
                    let src = self.get_reg(m.reg, bits);
                    let msk = mask(bits);
                    let (r, cf) = if opc < 0xA8 {
                        let wide = ((dst as u64) << bits) | src as u64;
                        let sh = wide << count;
                        (((sh >> bits) as u32) & msk, (wide >> (2 * bits - count)) & 1 != 0)
                    } else {
                        let wide = ((src as u64) << bits) | dst as u64;
                        ((((wide >> count) as u32) & msk), (dst >> (count - 1)) & 1 != 0)
                    };
                    self.write_op(m.rm, bits, r)?;
                    self.set_flag(CF, cf);
                    self.set_szp(r, bits);
                    self.set_flag(OF, (dst ^ r) & sign_bit(bits) != 0);
                }
            }
            // bsf / bsr
            0xBC | 0xBD => {
                let m = self.modrm(&p)?;
                let v = self.read_op(m.rm, bits)? & mask(bits);
                self.set_flag(ZF, v == 0);
                if v != 0 {
                    let idx = if opc == 0xBC { v.trailing_zeros() } else { 31 - v.leading_zeros() };
                    self.set_reg(m.reg, bits, idx);
                }
            }
            // cmpxchg r/m, r
            0xB0 | 0xB1 => {
                let b = if opc == 0xB0 { 8 } else { bits };
                let m = self.modrm(&p)?;
                let dst = self.read_op(m.rm, b)?;
                let acc = self.get_reg(EAX, b);
                self.alu(7, acc, dst, b); // cmp: sets all flags
                if acc == dst {
                    let src = self.get_reg(m.reg, b);
                    self.write_op(m.rm, b, src)?;
                } else {
                    self.set_reg(EAX, b, dst);
                }
            }
            // xadd r/m, r
            0xC0 | 0xC1 => {
                let b = if opc == 0xC0 { 8 } else { bits };
                let m = self.modrm(&p)?;
                let dst = self.read_op(m.rm, b)?;
                let src = self.get_reg(m.reg, b);
                let sum = self.alu(0, dst, src, b);
                self.set_reg(m.reg, b, dst);
                self.write_op(m.rm, b, sum)?;
            }
            // cmpxchg8b m64
            0xC7 => {
                let m = self.modrm(&p)?;
                let Op::Mem(addr) = m.rm else { return Err(self.invalid(start)) };
                if m.reg != 1 {
                    return Err(self.invalid(start));
                }
                let cur = (self.mem.read(addr, 4)? as u64) | ((self.mem.read(addr + 4, 4)? as u64) << 32);
                let want = (self.regs[EAX] as u64) | ((self.regs[EDX] as u64) << 32);
                if cur == want {
                    self.mem.write(addr, 4, self.regs[EBX])?;
                    self.mem.write(addr + 4, 4, self.regs[ECX])?;
                    self.set_flag(ZF, true);
                } else {
                    self.regs[EAX] = cur as u32;
                    self.regs[EDX] = (cur >> 32) as u32;
                    self.set_flag(ZF, false);
                }
            }
            _ => {
                if !self.exec_mmx(opc, &p)? {
                    return Err(self.invalid(start));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: u32 = 0x1000;

    /// Load `code` at 0x1000 (data scratch space at 0x2000..) and run until it stops.
    fn run(code: &[u8]) -> (Emu, Stop) {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x10000);
        e.mem.write_bytes(CODE, code).unwrap();
        e.eip = CODE;
        e.regs[ESP] = 0x10000;
        let stop = e.run(100_000);
        (e, stop)
    }

    fn run_ok(code: &[u8]) -> Emu {
        let (e, stop) = run(code);
        assert_eq!(stop, Stop::Halt, "program did not halt cleanly");
        e
    }

    #[test]
    fn mov_add_sub() {
        let e = run_ok(&[0xB8, 5, 0, 0, 0, 0x83, 0xC0, 3, 0x83, 0xE8, 1, 0xF4]);
        assert_eq!(e.regs[EAX], 7);
    }

    #[test]
    fn counted_loop_sums_1_to_10() {
        let e = run_ok(&[
            0xB9, 10, 0, 0, 0, // mov ecx, 10
            0x31, 0xC0, // xor eax, eax
            0x01, 0xC8, // L: add eax, ecx
            0x49, // dec ecx
            0x75, 0xFB, // jnz L
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 55);
        assert!(e.eflags & ZF != 0);
    }

    #[test]
    fn call_and_ret() {
        let e = run_ok(&[
            0xE8, 1, 0, 0, 0, // call f
            0xF4, // hlt
            0xB8, 0x34, 0x12, 0, 0, // f: mov eax, 0x1234
            0xC3,
        ]);
        assert_eq!(e.regs[EAX], 0x1234);
        assert_eq!(e.regs[ESP], 0x10000);
    }

    #[test]
    fn push_pop() {
        let e = run_ok(&[0x68, 0x44, 0x33, 0x22, 0x11, 0x5B, 0xF4]);
        assert_eq!(e.regs[EBX], 0x11223344);
        assert_eq!(e.regs[ESP], 0x10000);
    }

    #[test]
    fn unsigned_div_and_mul() {
        let e = run_ok(&[
            0xB8, 100, 0, 0, 0, 0xB9, 7, 0, 0, 0, 0x31, 0xD2, 0xF7, 0xF1, 0xF4,
        ]);
        assert_eq!((e.regs[EAX], e.regs[EDX]), (14, 2));

        let e = run_ok(&[
            0xB8, 0, 0, 1, 0, 0xB9, 0, 0, 1, 0, 0xF7, 0xE1, 0xF4,
        ]);
        assert_eq!((e.regs[EDX], e.regs[EAX]), (1, 0));
        assert!(e.eflags & CF != 0);
    }

    #[test]
    fn signed_idiv() {
        // eax = -7, cdq, idiv 2 -> q = -3, r = -1
        let e = run_ok(&[
            0xB8, 0xF9, 0xFF, 0xFF, 0xFF, 0x99, 0xB9, 2, 0, 0, 0, 0xF7, 0xF9, 0xF4,
        ]);
        assert_eq!(e.regs[EAX] as i32, -3);
        assert_eq!(e.regs[EDX] as i32, -1);
    }

    #[test]
    fn divide_by_zero_stops() {
        let (_, stop) = run(&[0x31, 0xD2, 0x31, 0xC9, 0xF7, 0xF1, 0xF4]);
        assert_eq!(stop, Stop::DivideError(CODE + 4));
    }

    #[test]
    fn shifts() {
        let e = run_ok(&[
            0xB8, 0, 0, 0, 0x80, 0xC1, 0xF8, 4, // sar eax, 4
            0xBB, 1, 0, 0, 0, 0xC1, 0xE3, 31, // shl ebx, 31
            0xB9, 0, 0, 0, 0x80, 0xC1, 0xE9, 31, // shr ecx, 31
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 0xF800_0000);
        assert_eq!(e.regs[EBX], 0x8000_0000);
        assert_eq!(e.regs[ECX], 1);
    }

    #[test]
    fn rep_stosd_fills_memory() {
        let e = run_ok(&[
            0xBF, 0x00, 0x20, 0, 0, // mov edi, 0x2000
            0xB9, 4, 0, 0, 0, // mov ecx, 4
            0xB8, 0xDD, 0xCC, 0xBB, 0xAA, // mov eax, 0xAABBCCDD
            0xFC, 0xF3, 0xAB, 0xF4,
        ]);
        for i in 0..4 {
            assert_eq!(e.mem.read(0x2000 + i * 4, 4).unwrap(), 0xAABB_CCDD);
        }
        assert_eq!(e.regs[EDI], 0x2010);
        assert_eq!(e.regs[ECX], 0);
    }

    #[test]
    fn rep_movsb_copies() {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x10000);
        e.mem.write_bytes(0x2000, b"abcdef").unwrap();
        e.mem.write_bytes(
            CODE,
            &[0xBE, 0, 0x20, 0, 0, 0xBF, 0, 0x30, 0, 0, 0xB9, 6, 0, 0, 0, 0xF3, 0xA4, 0xF4],
        )
        .unwrap();
        e.eip = CODE;
        assert_eq!(e.run(1000), Stop::Halt);
        assert_eq!(e.mem.read_bytes(0x3000, 6).unwrap(), b"abcdef");
    }

    #[test]
    fn repne_scasb_finds_nul() {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x10000);
        e.mem.write_bytes(0x2000, b"hello\0").unwrap();
        e.mem
            .write_bytes(
                CODE,
                &[0xBF, 0, 0x20, 0, 0, 0xB9, 0xFF, 0, 0, 0, 0x31, 0xC0, 0xF2, 0xAE, 0xF4],
            )
            .unwrap();
        e.eip = CODE;
        assert_eq!(e.run(1000), Stop::Halt);
        assert_eq!(e.regs[EDI], 0x2006);
    }

    #[test]
    fn sib_addressing() {
        let e = run_ok(&[
            0xBB, 0, 0x20, 0, 0, // mov ebx, 0x2000
            0xBE, 2, 0, 0, 0, // mov esi, 2
            0xC7, 0x44, 0xB3, 0x08, 0xEF, 0xBE, 0xAD, 0xDE, // mov [ebx+esi*4+8], 0xdeadbeef
            0x8B, 0x44, 0xB3, 0x08, // mov eax, [ebx+esi*4+8]
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 0xDEAD_BEEF);
        assert_eq!(e.mem.read(0x2010, 4).unwrap(), 0xDEAD_BEEF);
    }

    #[test]
    fn signed_vs_unsigned_conditions() {
        let e = run_ok(&[
            0xB8, 0xFF, 0xFF, 0xFF, 0xFF, // mov eax, -1
            0xBB, 1, 0, 0, 0, // mov ebx, 1
            0x39, 0xD8, // cmp eax, ebx
            0x0F, 0x9C, 0xC1, // setl cl
            0x0F, 0x92, 0xC2, // setb dl
            0xF4,
        ]);
        assert_eq!(e.regs[ECX] & 0xff, 1, "-1 < 1 signed");
        assert_eq!(e.regs[EDX] & 0xff, 0, "0xffffffff is not below 1 unsigned");
    }

    #[test]
    fn cmov() {
        let e = run_ok(&[
            0xB8, 1, 0, 0, 0, 0xBB, 9, 0, 0, 0, 0x39, 0xC0, // cmp eax, eax (ZF=1)
            0x0F, 0x44, 0xC3, // cmove eax, ebx
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 9);
    }

    #[test]
    fn movzx_movsx() {
        let e = run_ok(&[
            0xB8, 0x80, 0xFF, 0xFF, 0xFF, 0x0F, 0xBE, 0xD8, 0x0F, 0xB6, 0xC8, 0xF4,
        ]);
        assert_eq!(e.regs[EBX], 0xFFFF_FF80);
        assert_eq!(e.regs[ECX], 0x80);
    }

    #[test]
    fn sixteen_bit_ops_preserve_upper_half() {
        let e = run_ok(&[
            0xB8, 0x78, 0x56, 0x34, 0x12, // mov eax, 0x12345678
            0x66, 0xB8, 0xFF, 0xFF, // mov ax, 0xffff
            0x66, 0x83, 0xC0, 0x01, // add ax, 1
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 0x1234_0000);
        assert!(e.eflags & ZF != 0 && e.eflags & CF != 0);
    }

    #[test]
    fn high_byte_registers() {
        // mov eax,0; mov ah,0x12; mov al,0x34
        let e = run_ok(&[0x31, 0xC0, 0xB4, 0x12, 0xB0, 0x34, 0xF4]);
        assert_eq!(e.regs[EAX], 0x1234);
    }

    #[test]
    fn imul_three_operand() {
        // mov eax,-3; imul eax,eax,5
        let e = run_ok(&[0xB8, 0xFD, 0xFF, 0xFF, 0xFF, 0x6B, 0xC0, 5, 0xF4]);
        assert_eq!(e.regs[EAX] as i32, -15);
    }

    #[test]
    fn adc_and_sbb_chain_carry() {
        // 64-bit add: (1:ffffffff) + (0:1) -> (2:0)
        let e = run_ok(&[
            0xB8, 0xFF, 0xFF, 0xFF, 0xFF, // mov eax, 0xffffffff
            0xBA, 1, 0, 0, 0, // mov edx, 1
            0x83, 0xC0, 1, // add eax, 1
            0x83, 0xD2, 0, // adc edx, 0
            0xF4,
        ]);
        assert_eq!((e.regs[EDX], e.regs[EAX]), (2, 0));
    }

    #[test]
    fn inc_preserves_carry() {
        // stc; inc eax; -> CF still set
        let e = run_ok(&[0xF9, 0x40, 0xF4]);
        assert!(e.eflags & CF != 0);
    }

    #[test]
    fn fs_segment_reads_teb() {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x10000);
        e.fs_base = 0x2000;
        e.mem.write(0x2018, 4, 0xCAFE).unwrap();
        e.mem.write_bytes(CODE, &[0x64, 0xA1, 0x18, 0, 0, 0, 0xF4]).unwrap();
        e.eip = CODE;
        assert_eq!(e.run(10), Stop::Halt);
        assert_eq!(e.regs[EAX], 0xCAFE);
    }

    #[test]
    fn leave_and_frame() {
        // push ebp; mov ebp,esp; sub esp,16; leave
        let e = run_ok(&[0x55, 0x89, 0xE5, 0x83, 0xEC, 0x10, 0xC9, 0xF4]);
        assert_eq!(e.regs[ESP], 0x10000);
    }

    #[test]
    fn indirect_call_through_memory() {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x10000);
        e.mem.write(0x2000, 4, CODE + 8).unwrap();
        // call [0x2000]; hlt; (pad) ; mov eax, 77; ret
        e.mem
            .write_bytes(
                CODE,
                &[0xFF, 0x15, 0, 0x20, 0, 0, 0xF4, 0x90, 0xB8, 77, 0, 0, 0, 0xC3],
            )
            .unwrap();
        e.eip = CODE;
        e.regs[ESP] = 0x10000;
        assert_eq!(e.run(100), Stop::Halt);
        assert_eq!(e.regs[EAX], 77);
    }

    #[test]
    fn bit_test_family_with_register_offsets() {
        let e = run_ok(&[
            0xB8, 0, 0, 0, 0, // mov eax, 0
            0xBB, 5, 0, 0, 0, // mov ebx, 5
            0x0F, 0xAB, 0xD8, // bts eax, ebx   -> eax = 0x20, CF = 0
            0x9C, 0x59, // pushfd; pop ecx  (capture flags)
            0x0F, 0xA3, 0xD8, // bt eax, ebx    -> CF = 1
            0x9C, 0x5A, // pushfd; pop edx
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 0x20);
        assert_eq!(e.regs[ECX] & CF, 0, "bit was clear");
        assert_eq!(e.regs[EDX] & CF, CF, "bit is now set");
    }

    #[test]
    fn bit_test_on_memory_can_reach_past_the_operand() {
        // bts [ebx], ecx with ecx = 35 touches bit 3 of the *second* dword.
        let e = run_ok(&[
            0xBB, 0, 0x20, 0, 0, // mov ebx, 0x2000
            0xB9, 35, 0, 0, 0, // mov ecx, 35
            0x0F, 0xAB, 0x0B, // bts [ebx], ecx
            0xF4,
        ]);
        assert_eq!(e.mem.read(0x2000, 4).unwrap(), 0);
        assert_eq!(e.mem.read(0x2004, 4).unwrap(), 8);
    }

    #[test]
    fn bit_test_immediates_reset_and_complement() {
        let e = run_ok(&[
            0xB8, 0xFF, 0, 0, 0, // mov eax, 0xff
            0x0F, 0xBA, 0xF0, 3, // btr eax, 3 -> 0xf7, CF = 1
            0x0F, 0xBA, 0xF8, 0, // btc eax, 0 -> 0xf6
            0x0F, 0xBA, 0xE8, 8, // bts eax, 8 -> 0x1f6
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 0x1F6);
    }

    #[test]
    fn bsf_bsr_and_zero_source() {
        let e = run_ok(&[
            0xB8, 0x50, 0, 0, 0, // mov eax, 0x50
            0xB9, 0x77, 0, 0, 0, // mov ecx, 0x77 (must survive a zero source)
            0x0F, 0xBC, 0xD0, // bsf edx, eax -> 4
            0x0F, 0xBD, 0xD8, // bsr ebx, eax -> 6
            0x31, 0xF6, // xor esi, esi
            0x0F, 0xBC, 0xCE, // bsf ecx, esi (source 0): ZF set, ecx untouched
            0xF4,
        ]);
        assert_eq!((e.regs[EDX], e.regs[EBX]), (4, 6));
        assert_eq!(e.regs[ECX], 0x77);
        assert!(e.eflags & ZF != 0);
    }

    #[test]
    fn shld_and_shrd() {
        let e = run_ok(&[
            0xB8, 0x78, 0x56, 0x34, 0x12, // eax = 0x12345678
            0xBB, 0xF0, 0xDE, 0xBC, 0x9A, // ebx = 0x9abcdef0
            0x0F, 0xA4, 0xD8, 8, // shld eax, ebx, 8
            0xB9, 0x78, 0x56, 0x34, 0x12, // ecx = 0x12345678
            0x0F, 0xAC, 0xD9, 8, // shrd ecx, ebx, 8
            0xF4,
        ]);
        assert_eq!(e.regs[EAX], 0x3456_789A);
        assert_eq!(e.regs[ECX], 0xF012_3456);
    }

    #[test]
    fn cmpxchg_success_and_failure_with_lock_prefix() {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x10000);
        e.mem.write(0x2000, 4, 5).unwrap();
        e.mem
            .write_bytes(
                CODE,
                &[
                    0xB8, 5, 0, 0, 0, // eax = 5 (matches)
                    0xBB, 9, 0, 0, 0, // ebx = 9
                    0xF0, 0x0F, 0xB1, 0x1D, 0, 0x20, 0, 0, // lock cmpxchg [0x2000], ebx
                    0x9C, 0x59, // flags -> ecx
                    0xB8, 7, 0, 0, 0, // eax = 7 (no longer matches 9)
                    0x0F, 0xB1, 0x1D, 0, 0x20, 0, 0, // cmpxchg [0x2000], ebx -> fails, eax = 9
                    0x9C, 0x5A, // flags -> edx
                    0xF4,
                ],
            )
            .unwrap();
        e.eip = CODE;
        e.regs[ESP] = 0x10000;
        assert_eq!(e.run(100), Stop::Halt);
        assert_eq!(e.mem.read(0x2000, 4).unwrap(), 9, "exchange happened on the first, matching compare");
        assert!(e.regs[ECX] & ZF != 0, "ZF set on success");
        assert_eq!(e.regs[EAX], 9, "failed compare loads the memory value into eax");
        assert!(e.regs[EDX] & ZF == 0, "ZF clear on failure");
    }

    #[test]
    fn xadd_and_multibyte_nop() {
        let e = run_ok(&[
            0xB8, 1, 0, 0, 0, 0xBB, 2, 0, 0, 0, // eax = 1, ebx = 2
            0x0F, 0xC1, 0xD8, // xadd eax, ebx -> eax = 3, ebx = 1
            0x0F, 0x1F, 0x44, 0x00, 0x00, // nopl 0(%eax,%eax,1): five bytes, no effect
            0xB9, 0x2A, 0, 0, 0, // mov ecx, 42 (proves the nop consumed exactly its bytes)
            0xF4,
        ]);
        assert_eq!((e.regs[EAX], e.regs[EBX], e.regs[ECX]), (3, 1, 42));
    }

    #[test]
    fn segment_register_push_pop_and_mov() {
        let e = run_ok(&[
            0xB8, 0x78, 0x56, 0x34, 0x12, // eax = 0x12345678
            0x1E, 0x06, // push ds; push es
            0xB8, 0, 0, 0, 0, // clobber eax
            0x07, 0x1F, // pop es; pop ds  (what Miles does around its inline asm)
            0x8C, 0xDB, // mov ebx, ds
            0x8C, 0xC9, // mov ecx, cs
            0x0F, 0xA0, 0x5A, // push fs; pop edx
            0xF4,
        ]);
        assert_eq!(e.regs[ESP], 0x10000, "pushes and pops balance");
        assert_eq!((e.regs[EBX], e.regs[ECX], e.regs[EDX]), (0x23, 0x1B, 0x3B));
    }

    #[test]
    fn mov_to_a_segment_register_is_harmless() {
        let e = run_ok(&[0x8C, 0xD8, 0x8E, 0xC0, 0x8E, 0xD8, 0xB8, 7, 0, 0, 0, 0xF4]); // mov eax,ds; mov es,ax; mov ds,ax
        assert_eq!(e.regs[EAX], 7);
    }

    #[test]
    fn bswap() {
        let e = run_ok(&[0xB8, 0x78, 0x56, 0x34, 0x12, 0x0F, 0xC8, 0xF4]);
        assert_eq!(e.regs[EAX], 0x7856_3412);
    }

    #[test]
    fn unmapped_read_reports_fault_address() {
        let (_, stop) = run(&[0xA1, 0, 0, 0, 0x90]);
        assert_eq!(stop, Stop::Fault(Fault(0x9000_0000)));
    }

    #[test]
    fn unknown_opcode_reports_location() {
        let (_, stop) = run(&[0x0F, 0x0B]);
        assert!(matches!(stop, Stop::InvalidOpcode { eip: CODE, .. }));
    }

    #[test]
    fn step_limit() {
        let (_, stop) = {
            let mut e = Emu::new();
            e.mem.map(CODE, 0x1000);
            e.mem.write_bytes(CODE, &[0xEB, 0xFE]).unwrap(); // jmp $
            e.eip = CODE;
            let s = e.run(50);
            (e, s)
        };
        assert_eq!(stop, Stop::StepLimit);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release bench_loop -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_loop() {
        // loop: add eax,[esp+4]; mov [esp+8],eax; lea edx,[eax+ecx*2]; cmp edx,ebx; dec ecx; jnz loop
        let code: Vec<u8> = vec![
            0x03, 0x44, 0x24, 0x04, // add eax,[esp+4]
            0x89, 0x44, 0x24, 0x08, // mov [esp+8],eax
            0x8D, 0x14, 0x48, // lea edx,[eax+ecx*2]
            0x39, 0xDA, // cmp edx,ebx
            0x49, // dec ecx
            0x75, 0xF0, // jnz loop
        ];
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x20000);
        e.mem.write_bytes(0x10000, &code).unwrap();
        e.eip = 0x10000;
        e.regs[ESP] = 0x20000;
        e.regs[ECX] = 10_000_000;
        let t = std::time::Instant::now();
        let stop = e.run(200_000_000);
        let secs = t.elapsed().as_secs_f64();
        eprintln!("{stop:?}: {} steps in {secs:.2}s = {:.1}M steps/s", e.steps, e.steps as f64 / secs / 1e6);
    }
}
