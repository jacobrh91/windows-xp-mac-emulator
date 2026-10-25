//! Guest threads, scheduled by the emulator on one host thread.
//!
//! There is one CPU. A thread is a saved copy of its register state; switching swaps them in
//! and out. Threads are preempted every `quantum` instructions and switch immediately when
//! they block. Blocking calls (waits, sleeps, contended locks) do not park the thread:
//! the API returns `Stop::Blocked`, the call's stub address stays in EIP, and the scheduler
//! simply retries it when the thread next runs. When every thread is blocked the host is
//! asked for input, asked to sleep until the next deadline, or told it is a deadlock.

use crate::cpu::{Emu, Res, Stop, EAX, ESP};
use crate::cpu::fpu::Fpu;
use crate::win32::seh::SehState;
use crate::win32::vfs::{Obj, INVALID_HANDLE};
use crate::win32::{Handler, PEB_ADDR};

pub const MAIN_THREAD_ID: u32 = 0x500;
const WAIT_TIMEOUT: u32 = 258;
const INFINITE: u32 = 0xFFFF_FFFF;
const STILL_ACTIVE: u32 = 259;
const DEFAULT_STACK: u32 = 0x10_0000;
const CREATE_SUSPENDED: u32 = 4;
/// Instructions a thread runs before the scheduler offers the CPU to another.
pub const DEFAULT_QUANTUM: u32 = 10_000;

/// Why a thread cannot make progress right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Waiting for a window message (or other host input).
    Input,
    /// Waiting for a time (milliseconds on the emulator's clock) or for a signal before it.
    Until(u64),
    /// Waiting for something only another thread can provide.
    Forever,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ready,
    Suspended,
    Finished,
}

/// Everything that makes up a thread's CPU state while it is not the running one.
pub struct CpuState {
    regs: [u32; 8],
    eip: u32,
    eflags: u32,
    fs_base: u32,
    fpu: Fpu,
    nested: Vec<(u32, u32)>,
    seh: Vec<SehState>,
    pending_wait: Option<(u32, u32, u64)>,
}

pub struct Thread {
    pub id: u32,
    pub handle: u32,
    state: State,
    suspend_count: u32,
    /// Saved state; only meaningful while this is not the running thread.
    cpu: CpuState,
    pub tls: Vec<u32>,
    block: Option<Block>,
}

fn empty_cpu() -> CpuState {
    CpuState { regs: [0; 8], eip: 0, eflags: 2, fs_base: 0, fpu: Fpu::new(), nested: Vec::new(), seh: Vec::new(), pending_wait: None }
}

impl Emu {
    pub fn cur_tid(&self) -> u32 {
        self.threads.get(self.current).map_or(MAIN_THREAD_ID, |t| t.id)
    }

    /// Threads are only tracked once a second one exists.
    fn ensure_main_thread(&mut self) {
        if !self.threads.is_empty() {
            return;
        }
        let tls = std::mem::take(&mut self.win.tls);
        self.threads.push(Thread {
            id: MAIN_THREAD_ID,
            handle: INVALID_HANDLE,
            state: State::Ready,
            suspend_count: 0,
            cpu: empty_cpu(), // the running thread's state lives in the CPU itself
            tls,
            block: None,
        });
        self.current = 0;
    }

    fn save_cpu(&mut self) -> CpuState {
        CpuState {
            regs: self.regs,
            eip: self.eip,
            eflags: self.eflags,
            fs_base: self.fs_base,
            fpu: self.fpu.clone(),
            nested: std::mem::take(&mut self.nested),
            seh: std::mem::take(&mut self.seh),
            pending_wait: self.pending_wait.take(),
        }
    }

    fn load_cpu(&mut self, s: CpuState) {
        self.regs = s.regs;
        self.eip = s.eip;
        self.eflags = s.eflags;
        self.fs_base = s.fs_base;
        self.fpu = s.fpu;
        self.nested = s.nested;
        self.seh = s.seh;
        self.pending_wait = s.pending_wait;
    }

    fn switch_to(&mut self, idx: usize) {
        if idx == self.current {
            return;
        }
        let out = self.save_cpu();
        self.threads[self.current].cpu = out;
        let incoming = std::mem::replace(&mut self.threads[idx].cpu, empty_cpu());
        self.load_cpu(incoming);
        self.current = idx;
    }

    /// How long the host may sleep (ms) after a run ended because every thread is waiting:
    /// until the earliest thread deadline, timer or audio buffer, capped at `max`.
    pub fn idle_ms(&self, max: u64) -> u64 {
        let now = self.win.now_ms();
        let due = [self.wake_at, self.win.audio.next_due()].into_iter().flatten().min();
        due.map_or(max, |t| t.saturating_sub(now).min(max))
    }

    /// One line per thread: what it is doing and what it waits for (a debugging aid).
    pub fn dump_threads(&self) {
        for (i, t) in self.threads.iter().enumerate() {
            let (eip, pending) = if i == self.current { (self.eip, self.pending_wait) } else { (t.cpu.eip, t.cpu.pending_wait) };
            let at = self.win.locate(eip).unwrap_or_else(|| format!("{eip:#x}"));
            let esp = if i == self.current { self.regs[4] } else { t.cpu.regs[4] };
            let mut frames = Vec::new();
            for k in 0..256u32 {
                if let Ok(v) = self.mem.read(esp.wrapping_add(4 * k), 4) {
                    if let Some(loc) = self.win.locate(v) {
                        if !loc.starts_with("0x") && frames.len() < 10 {
                            frames.push(loc);
                        }
                    }
                }
            }
            eprintln!("[thread {}]   stack: {}", t.id, frames.join(" "));
            eprintln!("[thread {}] {:?} block={:?} at {} wait={:?}{}", t.id, t.state, t.block, at, pending, if i == self.current { " (current)" } else { "" });
        }
    }

    /// The next thread after `from` that can run (blocked threads count: they re-poll).
    fn next_ready(&self, from: usize) -> Option<usize> {
        let n = self.threads.len();
        (1..=n).map(|k| (from + k) % n).find(|&i| self.threads[i].state == State::Ready)
    }

    /// Offer the CPU to the next thread. Called when the running thread has had its slice.
    pub(crate) fn rotate(&mut self) {
        if let Some(next) = self.next_ready(self.current) {
            self.switch_to(next);
        }
    }

    /// Create a thread; returns its handle.
    pub(crate) fn create_thread(&mut self, start: u32, param: u32, stack_size: u32, suspended: bool) -> Res<u32> {
        self.ensure_main_thread();
        let size = if stack_size == 0 { DEFAULT_STACK } else { stack_size.clamp(0x4000, 0x400_0000) };
        let stack = self.alloc_virtual(size);
        let top = stack + size;
        let id = MAIN_THREAD_ID + self.threads.len() as u32;
        let handle = self.win.vfs.create_thread();

        // Its own thread information block.
        let teb = self.alloc_virtual(0x1000);
        for (off, v) in [(0u32, 0xFFFF_FFFFu32), (4, top), (8, stack), (0x18, teb), (0x20, 0x400), (0x24, id), (0x30, PEB_ADDR)] {
            self.mem.write(teb + off, 4, v)?;
        }
        // Returning from the thread function lands in the exit stub with EAX as the exit code.
        let exit_stub = self.win.resolve("__emu", "thread_exit");
        let esp = top - 8;
        self.mem.write(esp, 4, exit_stub)?;
        self.mem.write(esp + 4, 4, param)?;
        let mut regs = [0u32; 8];
        regs[ESP] = esp;
        self.threads.push(Thread {
            id,
            handle,
            state: if suspended { State::Suspended } else { State::Ready },
            suspend_count: suspended as u32,
            cpu: CpuState { regs, eip: start, eflags: 2, fs_base: teb, fpu: Fpu::new(), nested: Vec::new(), seh: Vec::new(), pending_wait: None },
            tls: Vec::new(),
            block: None,
        });
        Ok(handle)
    }

    fn thread_index_by_handle(&self, h: u32) -> Option<usize> {
        self.threads.iter().position(|t| t.handle == h)
    }

    fn finish_thread(&mut self, idx: usize, code: u32) {
        self.threads[idx].state = State::Finished;
        let h = self.threads[idx].handle;
        if let Some(Obj::Thread { done, exit_code, .. }) = self.win.vfs.get_mut(h) {
            *done = true;
            *exit_code = code;
        }
    }

    /// Route a stop raised while running: scheduling stops are handled here and `None` means
    /// "carry on"; anything else is returned to the caller of `run`.
    pub(crate) fn route_stop(&mut self, stop: Stop) -> Option<Stop> {
        match stop {
            Stop::Blocked(b) => self.handle_block(b),
            Stop::ThreadExit => match self.next_ready(self.current) {
                Some(next) => {
                    self.switch_to(next);
                    None
                }
                None => Some(Stop::Exit(0)),
            },
            other => Some(other),
        }
    }

    fn block_to_stop(&self, b: &Block) -> Stop {
        match b {
            Block::Input => Stop::WaitForInput,
            Block::Until(_) => Stop::Sleeping,
            Block::Forever => Stop::Unimplemented("deadlock: the only thread is waiting for something that can never happen".into()),
        }
    }

    fn handle_block(&mut self, b: Block) -> Option<Stop> {
        let others = self.threads.len() > 1 && self.next_ready_other().is_some();
        if !others {
            // The only runnable thread is stuck. A timer or an audio buffer may be what it needs.
            if self.pump_devices().unwrap_or(false) {
                return None;
            }
            self.wake_at = match b {
                Block::Until(t) => Some(t),
                _ => None,
            };
            return Some(self.block_to_stop(&b));
        }
        self.threads[self.current].block = Some(b);
        self.stall += 1;
        let runnable = self.threads.iter().filter(|t| t.state == State::Ready).count() as u32;
        if self.stall > runnable * 2 {
            // Everyone has been polled and nobody got anywhere. Device events (timers, audio
            // buffers finishing) may be what unblocks them.
            if self.pump_devices().unwrap_or(false) {
                self.stall = 0;
            } else {
                let blocks: Vec<Block> = self.threads.iter().filter(|t| t.state == State::Ready).filter_map(|t| t.block.clone()).collect();
                // The earliest any of them can make progress, so the host sleeps just that long.
                self.wake_at = blocks.iter().filter_map(|b| if let Block::Until(t) = b { Some(*t) } else { None }).min();
                if blocks.contains(&Block::Input) {
                    return Some(Stop::WaitForInput);
                }
                if blocks.iter().any(|b| matches!(b, Block::Until(_))) {
                    return Some(Stop::Sleeping);
                }
                let ids: Vec<String> = self.threads.iter().filter(|t| t.state == State::Ready).map(|t| format!("{:#x}", t.id)).collect();
                return Some(Stop::Unimplemented(format!("deadlock: every thread ({}) is waiting on another", ids.join(", "))));
            }
        }
        self.rotate();
        None
    }

    fn next_ready_other(&self) -> Option<usize> {
        self.next_ready(self.current).filter(|&i| i != self.current)
    }

    /// Absolute deadline for the blocking call in progress (`None` = infinite). The same call
    /// is retried many times, so the deadline is remembered and keyed by the call's position.
    pub(crate) fn wait_deadline(&mut self, ms: u32) -> Option<u64> {
        if ms == INFINITE {
            return None;
        }
        let key = (self.eip, self.regs[ESP]);
        if let Some((e, s, d)) = self.pending_wait {
            if (e, s) == key {
                return Some(d);
            }
        }
        let d = self.win.now_ms() + ms as u64;
        self.pending_wait = Some((key.0, key.1, d));
        Some(d)
    }

    pub(crate) fn tls_values(&mut self) -> &mut Vec<u32> {
        match self.threads.get_mut(self.current) {
            Some(t) => &mut t.tls,
            None => &mut self.win.tls,
        }
    }
}

/// Wait on kernel objects. Satisfied waits consume auto-reset events and take mutexes.
pub fn wait_objects(e: &mut Emu, handles: &[u32], all: bool, ms: u32) -> Res<u32> {
    let me = e.cur_tid();
    let mut ready = Vec::new();
    for (i, &h) in handles.iter().enumerate() {
        let signaled = match e.win.vfs.get_mut(h) {
            Some(Obj::Event { signaled, .. }) => *signaled,
            Some(Obj::Mutex { owner, .. }) => *owner == 0 || *owner == me,
            Some(Obj::Thread { done, .. }) => *done,
            _ => true, // process handles, files and the like are always signaled
        };
        if signaled {
            ready.push(i as u32);
        }
    }
    let satisfied = if all { ready.len() == handles.len() } else { !ready.is_empty() };
    if satisfied {
        // Taking the objects: auto-reset events clear, mutexes are acquired.
        let taken: Vec<u32> = if all { (0..handles.len() as u32).collect() } else { vec![ready[0]] };
        for i in taken {
            match e.win.vfs.get_mut(handles[i as usize]) {
                Some(Obj::Event { signaled, manual }) if !*manual => *signaled = false,
                Some(Obj::Mutex { owner, count }) => {
                    *owner = me;
                    *count += 1;
                }
                _ => {}
            }
        }
        e.pending_wait = None;
        return Ok(ready.first().copied().unwrap_or(0));
    }
    match e.wait_deadline(ms) {
        Some(d) if e.win.now_ms() >= d => {
            e.pending_wait = None;
            Ok(WAIT_TIMEOUT)
        }
        Some(d) => Err(Stop::Blocked(Block::Until(d))),
        None => Err(Stop::Blocked(Block::Forever)),
    }
}

fn sleep(e: &mut Emu, ms: u32) -> Res<u32> {
    if ms == 0 {
        e.slice = e.quantum; // Sleep(0): give up the rest of the slice
        return Ok(0);
    }
    let d = e.wait_deadline(ms).unwrap_or(u64::MAX);
    if e.win.now_ms() >= d {
        e.pending_wait = None;
        Ok(0)
    } else {
        Err(Stop::Blocked(Block::Until(d)))
    }
}

// CRITICAL_SECTION: DebugInfo, LockCount, RecursionCount, OwningThread, LockSemaphore, SpinCount
const CS_RECURSION: u32 = 8;
const CS_OWNER: u32 = 12;

fn cs_init(e: &mut Emu, cs: u32) -> Res<()> {
    e.mem.write_bytes(cs, &[0; 24])?;
    e.mem.write(cs + 4, 4, 0xFFFF_FFFF)?; // LockCount: -1 = free
    Ok(())
}

fn cs_try_enter(e: &mut Emu, cs: u32) -> Res<bool> {
    let (owner, rec) = (e.mem.read(cs + CS_OWNER, 4)?, e.mem.read(cs + CS_RECURSION, 4)?);
    let me = e.cur_tid();
    if owner == 0 || owner == me {
        e.mem.write(cs + CS_OWNER, 4, me)?;
        e.mem.write(cs + CS_RECURSION, 4, rec + 1)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll == "kernel32" {
        if let Some(r) = lookup_suspend(name) {
            return Some(r);
        }
    }
    let r: (usize, Handler) = match (dll, name) {
        ("__emu", "thread_exit") => (0, |e, _| {
            let code = e.regs[EAX];
            if e.threads.is_empty() {
                return Err(Stop::Exit(code));
            }
            let cur = e.current;
            e.finish_thread(cur, code);
            Err(Stop::ThreadExit)
        }),
        ("kernel32", "CreateThread") => (6, |e, a| {
            let h = e.create_thread(a[2], a[3], a[1], a[4] & CREATE_SUSPENDED != 0)?;
            if a[5] != 0 {
                let id = e.threads.last().map_or(0, |t| t.id);
                e.mem.write(a[5], 4, id)?;
            }
            Ok(h)
        }),
        ("kernel32", "ExitThread") => (1, |e, a| {
            if e.threads.is_empty() || e.current == 0 {
                return Err(Stop::Exit(a[0]));
            }
            let cur = e.current;
            e.finish_thread(cur, a[0]);
            Err(Stop::ThreadExit)
        }),
        ("kernel32", "TerminateThread") => (2, |e, a| {
            let Some(i) = e.thread_index_by_handle(a[0]) else { return Ok(0) };
            e.finish_thread(i, a[1]);
            if i == e.current {
                return Err(Stop::ThreadExit);
            }
            Ok(1)
        }),
        ("kernel32", "GetCurrentThreadId") => (0, |e, _| Ok(e.cur_tid())),
        ("kernel32", "GetExitCodeThread") => (2, |e, a| {
            let code = match e.win.vfs.get(a[0]) {
                Some(Obj::Thread { done: true, exit_code, .. }) => *exit_code,
                Some(Obj::Thread { .. }) => STILL_ACTIVE,
                _ => return Ok(0),
            };
            e.mem.write(a[1], 4, code)?;
            Ok(1)
        }),
        ("kernel32", "SwitchToThread") => (0, |e, _| {
            e.slice = e.quantum;
            Ok(e.threads.len().min(1) as u32)
        }),
        ("kernel32", "Sleep") => (1, |e, a| sleep(e, a[0])),
        ("kernel32", "SleepEx") => (2, |e, a| sleep(e, a[0])),
        ("kernel32", "WaitForSingleObject") => (2, |e, a| wait_objects(e, &[a[0]], false, a[1])),
        ("kernel32", "WaitForSingleObjectEx") => (3, |e, a| wait_objects(e, &[a[0]], false, a[1])),
        ("kernel32", "WaitForMultipleObjects") => (4, |e, a| {
            let mut hs = Vec::new();
            for i in 0..a[0].min(64) {
                hs.push(e.mem.read(a[1] + 4 * i, 4)?);
            }
            wait_objects(e, &hs, a[2] != 0, a[3])
        }),
        ("kernel32", "TlsAlloc") => (0, |e, _| {
            let slot = e.win.tls_next;
            if slot >= 1088 {
                e.win.last_error = 8; // ERROR_NOT_ENOUGH_MEMORY
                return Ok(INFINITE);
            }
            e.win.tls_next += 1;
            Ok(slot)
        }),
        ("kernel32", "TlsFree") => (1, |_, _| Ok(1)),
        ("kernel32", "TlsGetValue") => (1, |e, a| Ok(e.tls_values().get(a[0] as usize).copied().unwrap_or(0))),
        ("kernel32", "TlsSetValue") => (2, |e, a| {
            if a[0] >= 1088 {
                return Ok(0);
            }
            let v = e.tls_values();
            if v.len() <= a[0] as usize {
                v.resize(a[0] as usize + 1, 0);
            }
            v[a[0] as usize] = a[1];
            Ok(1)
        }),
        ("kernel32", "InitializeCriticalSection") => (1, |e, a| {
            cs_init(e, a[0])?;
            Ok(0)
        }),
        ("kernel32", "InitializeCriticalSectionAndSpinCount") => (2, |e, a| {
            cs_init(e, a[0])?;
            Ok(1)
        }),
        ("kernel32", "DeleteCriticalSection") => (1, |_, _| Ok(0)),
        ("kernel32", "EnterCriticalSection") => (1, |e, a| {
            if cs_try_enter(e, a[0])? {
                Ok(0)
            } else {
                Err(Stop::Blocked(Block::Forever))
            }
        }),
        ("kernel32", "TryEnterCriticalSection") => (1, |e, a| Ok(cs_try_enter(e, a[0])? as u32)),
        ("kernel32", "LeaveCriticalSection") => (1, |e, a| {
            let rec = e.mem.read(a[0] + CS_RECURSION, 4)?;
            if rec > 1 {
                e.mem.write(a[0] + CS_RECURSION, 4, rec - 1)?;
            } else {
                e.mem.write(a[0] + CS_RECURSION, 4, 0)?;
                e.mem.write(a[0] + CS_OWNER, 4, 0)?;
            }
            Ok(0)
        }),
        _ => return None,
    };
    Some(r)
}

/// ResumeThread / SuspendThread need to know which of the two they are, so they are two
/// separate handlers rather than one shared arm.
pub fn lookup_suspend(name: &str) -> Option<(usize, Handler)> {
    match name {
        "ResumeThread" => Some((1, |e, a| {
            let Some(i) = e.thread_index_by_handle(a[0]) else { return Ok(0) };
            let t = &mut e.threads[i];
            let prev = t.suspend_count;
            t.suspend_count = t.suspend_count.saturating_sub(1);
            if t.suspend_count == 0 && t.state == State::Suspended {
                t.state = State::Ready;
            }
            Ok(prev)
        })),
        "SuspendThread" => Some((1, |e, a| {
            let Some(i) = e.thread_index_by_handle(a[0]) else { return Ok(0) };
            let t = &mut e.threads[i];
            let prev = t.suspend_count;
            t.suspend_count += 1;
            if t.state == State::Ready && i != e.current {
                t.state = State::Suspended;
            }
            Ok(prev)
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader;
    use crate::tests::util::{build_pe, Asm, CODE_VA};

    const K32: &[&str] = &[
        "CreateThread", "WaitForSingleObject", "ExitProcess", "EnterCriticalSection", "LeaveCriticalSection",
        "InitializeCriticalSection", "CreateEventA", "SetEvent", "Sleep", "TlsAlloc", "TlsSetValue", "TlsGetValue",
        "GetCurrentThreadId", "GetExitCodeThread",
    ];
    const CT: usize = 0;
    const WAIT: usize = 1;
    const EXIT: usize = 2;
    const ENTER: usize = 3;
    const LEAVE: usize = 4;
    const INIT_CS: usize = 5;
    const CREATE_EVENT: usize = 6;
    const SET_EVENT: usize = 7;
    const SLEEP: usize = 8;
    const TLS_ALLOC: usize = 9;
    const TLS_SET: usize = 10;
    const TLS_GET: usize = 11;
    const GET_TID: usize = 12;
    const GET_EXIT: usize = 13;

    fn data(emu: &Emu, slot: u32) -> u32 {
        emu.mem.read(crate::tests::util::data_va(slot), 4).unwrap()
    }

    fn run_with(image: &[u8], quantum: u32, steps: u64) -> (Emu, Stop) {
        let mut emu = Emu::new();
        emu.win.virtual_now = Some(0);
        emu.quantum = quantum;
        loader::load(&mut emu, image).unwrap();
        let stop = emu.run(steps);
        (emu, stop)
    }

    /// main: h = CreateThread(worker, 41); WaitForSingleObject(h); GetExitCodeThread; ExitProcess(code)
    /// worker(p): data[0] = p + 1; return p + 1
    fn join_program() -> Vec<u8> {
        build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.e(&[0x68]).data_va(1); // &tid
            a.e(&[0x6A, 0]).e(&[0x6A, 41]).push_label("worker").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x89, 0xC6]); // mov esi, eax
            a.e(&[0x6A, 0xFF, 0x56]).call(l.iat_va(0, WAIT)); // WaitForSingleObject(h, INFINITE)
            a.e(&[0x68]).data_va(2).e(&[0x56]).call(l.iat_va(0, GET_EXIT)); // GetExitCodeThread(h, &data[2])
            a.push_mem_data(2).call(l.iat_va(0, EXIT));
            a.label("worker");
            a.e(&[0x8B, 0x44, 0x24, 0x04, 0x40]).store(0).e(&[0xC2, 4, 0]);
            a.finish()
        })
    }

    #[test]
    fn a_thread_runs_and_a_waiter_gets_its_exit_code() {
        let (emu, stop) = run_with(&join_program(), 50, 100_000);
        assert_eq!(stop, Stop::Exit(42), "the exit code came back through WaitForSingleObject + GetExitCodeThread");
        assert_eq!(data(&emu, 0), 42, "the thread ran with its own parameter");
        assert_eq!(emu.threads.len(), 2);
    }

    /// worker spins forever incrementing data[0]; main spins until data[0] >= 1000, then exits 7.
    /// Both are busy loops: this only finishes if the scheduler preempts.
    fn preemption_program() -> Vec<u8> {
        build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0]).push_label("worker").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.label("spin");
            a.e(&[0x81, 0x3D]).data_va(0).e(&1000u32.to_le_bytes()); // cmp [data0], 1000
            a.jcc8(0x72, "spin"); // jb spin
            a.e(&[0x6A, 7]).call(l.iat_va(0, EXIT));
            a.label("worker");
            a.label("inc").e(&[0xFF, 0x05]).data_va(0).jmp8("inc");
            a.finish()
        })
    }

    #[test]
    fn busy_threads_are_preempted() {
        let (emu, stop) = run_with(&preemption_program(), 40, 1_000_000);
        assert_eq!(stop, Stop::Exit(7));
        assert!(data(&emu, 0) >= 1000);
    }

    /// Two threads (main and a worker) each add 1 to data[0] `ROUNDS` times with a
    /// read/add/write sequence that a preemption can split.
    const ROUNDS: u32 = 400;

    fn counter_program(locked: bool) -> Vec<u8> {
        build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            if locked {
                a.e(&[0x68]).data_va(8).call(l.iat_va(0, INIT_CS)); // InitializeCriticalSection(&cs)
            }
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0]).push_label("work").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x89, 0xC7]); // mov edi, eax (thread handle)
            a.e(&[0x6A, 0]).call_label("work"); // push a dummy parameter; work does `ret 4`
            a.e(&[0x6A, 0xFF, 0x57]).call(l.iat_va(0, WAIT));
            a.push_mem_data(0).call(l.iat_va(0, EXIT));
            a.label("work");
            a.e(&[0xBE]).e(&ROUNDS.to_le_bytes()); // mov esi, ROUNDS
            a.label("loop");
            if locked {
                a.e(&[0x68]).data_va(8).call(l.iat_va(0, ENTER));
            }
            a.e(&[0xA1]).data_va(0); // mov eax, [data0]
            a.e(&[0x40]); // inc eax
            a.store(0);
            if locked {
                a.e(&[0x68]).data_va(8).call(l.iat_va(0, LEAVE));
            }
            a.e(&[0x4E]).jcc8(0x75, "loop"); // dec esi; jnz loop
            a.e(&[0xC2, 4, 0]); // ret 4: pops the thread parameter (main pushes a dummy one)
            a.finish()
        })
    }

    #[test]
    fn a_critical_section_makes_a_split_read_modify_write_safe() {
        // Tiny quantum: threads are interrupted between the load and the store all the time.
        let (emu, stop) = run_with(&counter_program(true), 3, 5_000_000);
        assert_eq!(stop, Stop::Exit(2 * ROUNDS), "no lost updates");
        assert_eq!(data(&emu, 0), 2 * ROUNDS);
        assert_eq!(emu.mem.read(crate::tests::util::data_va(8) + 12, 4).unwrap(), 0, "lock released at the end");
    }

    #[test]
    fn without_the_lock_updates_are_lost_so_the_test_above_has_teeth() {
        let (_, stop) = run_with(&counter_program(false), 3, 5_000_000);
        let Stop::Exit(total) = stop else { panic!("{stop:?}") };
        assert!(total < 2 * ROUNDS, "unsynchronized increments should race (got {total})");
    }

    /// main: ev = CreateEvent(manual, unsignaled); t = CreateThread(waiter, ev); SetEvent(ev);
    /// Wait(t); ExitProcess(data[0]). waiter: Wait(ev, INFINITE); data[0] = 5.
    fn event_program() -> Vec<u8> {
        build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 1, 0x6A, 0]).call(l.iat_va(0, CREATE_EVENT));
            a.e(&[0x89, 0xC6]); // esi = event
            a.e(&[0x6A, 0, 0x6A, 0, 0x56]).push_label("waiter").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x89, 0xC7]); // edi = thread
            a.e(&[0x6A, 10]).call(l.iat_va(0, SLEEP)); // give the waiter time to block first
            a.e(&[0x56]).call(l.iat_va(0, SET_EVENT));
            a.e(&[0x6A, 0xFF, 0x57]).call(l.iat_va(0, WAIT));
            a.push_mem_data(0).call(l.iat_va(0, EXIT));
            a.label("waiter");
            a.e(&[0x6A, 0xFF, 0xFF, 0x74, 0x24, 0x08]).call(l.iat_va(0, WAIT)); // Wait(param, INFINITE)
            a.e(&[0xC7, 0x05]).data_va(0).e(&5u32.to_le_bytes()); // data[0] = 5
            a.e(&[0xC2, 4, 0]);
            a.finish()
        })
    }

    #[test]
    fn a_thread_blocked_on_an_event_wakes_when_it_is_signalled() {
        let mut emu = Emu::new();
        emu.win.virtual_now = Some(0);
        emu.quantum = 30;
        loader::load(&mut emu, &event_program()).unwrap();
        // main sleeps 10 ms; the waiter blocks on the event. Both are blocked: time must pass.
        assert_eq!(emu.run(100_000), Stop::Sleeping);
        assert_eq!(data(&emu, 0), 0, "the waiter is still blocked");
        emu.win.virtual_now = Some(10);
        assert_eq!(emu.run(100_000), Stop::Exit(5), "SetEvent woke the waiter, which finished before main exited");
    }

    #[test]
    fn sleep_blocks_until_the_clock_reaches_the_deadline() {
        let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 100]).call(l.iat_va(0, SLEEP));
            a.e(&[0x6A, 9]).call(l.iat_va(0, EXIT));
            a.finish()
        });
        let mut emu = Emu::new();
        emu.win.virtual_now = Some(0);
        loader::load(&mut emu, &image).unwrap();
        assert_eq!(emu.run(10_000), Stop::Sleeping);
        emu.win.virtual_now = Some(99);
        assert_eq!(emu.run(10_000), Stop::Sleeping, "one millisecond early");
        emu.win.virtual_now = Some(100);
        assert_eq!(emu.run(10_000), Stop::Exit(9));
    }

    #[test]
    fn waiting_forever_on_an_event_nobody_can_signal_is_reported_as_a_deadlock() {
        let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CREATE_EVENT));
            a.e(&[0x6A, 0xFF, 0x50]).call(l.iat_va(0, WAIT));
            a.finish()
        });
        let (_, stop) = run_with(&image, 100, 10_000);
        assert!(matches!(&stop, Stop::Unimplemented(m) if m.contains("deadlock")), "{stop:?}");
    }

    #[test]
    fn two_threads_waiting_on_each_other_are_a_deadlock_not_a_hang() {
        // main waits for the worker, the worker waits for an event nobody sets.
        let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CREATE_EVENT));
            a.e(&[0x89, 0xC6]);
            a.e(&[0x6A, 0, 0x6A, 0, 0x56]).push_label("w").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x6A, 0xFF, 0x50]).call(l.iat_va(0, WAIT));
            a.label("w");
            a.e(&[0x6A, 0xFF, 0xFF, 0x74, 0x24, 0x08]).call(l.iat_va(0, WAIT));
            a.e(&[0xC2, 4, 0]);
            a.finish()
        });
        let (_, stop) = run_with(&image, 20, 100_000);
        assert!(matches!(&stop, Stop::Unimplemented(m) if m.contains("deadlock")), "{stop:?}");
    }

    /// TLS slots are per thread: main stores 111, the worker sees 0 and stores 222; main still reads 111.
    fn tls_program() -> Vec<u8> {
        build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.call(l.iat_va(0, TLS_ALLOC)).store(5); // data[5] = slot
            a.e(&[0x6A, 111, 0x50]).call(l.iat_va(0, TLS_SET)); // TlsSetValue(slot, 111)
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0]).push_label("worker").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x89, 0xC7]);
            a.e(&[0x6A, 0xFF, 0x57]).call(l.iat_va(0, WAIT));
            a.push_mem_data(5).call(l.iat_va(0, TLS_GET)); // main's value afterwards
            a.store(1);
            a.push_mem_data(0).call(l.iat_va(0, EXIT)); // exit(worker's initial value)
            a.label("worker");
            a.push_mem_data(5).call(l.iat_va(0, TLS_GET)).store(0); // data[0] = what the worker sees
            a.e(&[0x68, 222, 0, 0, 0]).e(&[0xFF, 0x35]).data_va(5).call(l.iat_va(0, TLS_SET));
            a.e(&[0xC2, 4, 0]);
            a.finish()
        })
    }

    #[test]
    fn thread_local_storage_is_per_thread() {
        let (emu, stop) = run_with(&tls_program(), 25, 100_000);
        assert_eq!(stop, Stop::Exit(0), "the worker started with an empty slot");
        assert_eq!(data(&emu, 1), 111, "the worker's store did not touch main's value");
    }

    #[test]
    fn each_thread_has_its_own_id() {
        let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.call(l.iat_va(0, GET_TID)).store(1);
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0]).push_label("w").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x89, 0xC7, 0x6A, 0xFF, 0x57]).call(l.iat_va(0, WAIT));
            a.e(&[0x6A, 0]).call(l.iat_va(0, EXIT));
            a.label("w");
            a.call(l.iat_va(0, GET_TID)).store(2).e(&[0xC2, 4, 0]);
            a.finish()
        });
        let (emu, _) = run_with(&image, 30, 100_000);
        assert_eq!(data(&emu, 1), MAIN_THREAD_ID);
        assert_ne!(data(&emu, 2), MAIN_THREAD_ID);
        assert_ne!(data(&emu, 2), 0);
    }

    #[test]
    fn other_threads_keep_running_while_main_waits_for_input() {
        // worker: data[0] += 1, 100 times, then exits. main: GetMessage forever.
        let image = build_pe(&[("KERNEL32.dll", K32), ("user32.dll", &["GetMessageA"])], |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0]).push_label("w").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0]).e(&[0x68]).data_va(16).call(l.iat_va(1, 0)); // GetMessageA(&msg,0,0,0)
            a.e(&[0x6A, 0]).call(l.iat_va(0, EXIT));
            a.label("w");
            a.e(&[0xBE, 100, 0, 0, 0]);
            a.label("l").e(&[0xFF, 0x05]).data_va(0).e(&[0x4E]).jcc8(0x75, "l");
            a.e(&[0xC2, 4, 0]);
            a.finish()
        });
        let (emu, stop) = run_with(&image, 20, 1_000_000);
        assert_eq!(stop, Stop::WaitForInput, "asks the host for input only once nothing else can run");
        assert_eq!(data(&emu, 0), 100, "the worker finished its work in the meantime");
    }

    #[test]
    fn suspended_threads_do_not_run_until_resumed() {
        // CREATE_SUSPENDED thread would set data[0]; main checks it stays 0 while suspended.
        let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 0, 0x6A, 4, 0x6A, 0]).push_label("w").e(&[0x6A, 0, 0x6A, 0]).call(l.iat_va(0, CT));
            a.e(&[0x6A, 50]).call(l.iat_va(0, SLEEP));
            a.push_mem_data(0).call(l.iat_va(0, EXIT));
            a.label("w");
            a.e(&[0xC7, 0x05]).data_va(0).e(&1u32.to_le_bytes()).e(&[0xC2, 4, 0]);
            a.finish()
        });
        let mut emu = Emu::new();
        emu.win.virtual_now = Some(0);
        emu.quantum = 20;
        loader::load(&mut emu, &image).unwrap();
        assert_eq!(emu.run(100_000), Stop::Sleeping);
        emu.win.virtual_now = Some(50);
        assert_eq!(emu.run(100_000), Stop::Exit(0), "the suspended thread never ran");
        let _ = CODE_VA;
    }
}
