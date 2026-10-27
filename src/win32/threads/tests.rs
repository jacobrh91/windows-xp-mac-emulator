//! Tests of `threads`.

use super::*;
use crate::loader;
use crate::tests::util::{Asm, CODE_VA, build_pe};

const K32: &[&str] = &[
    "CreateThread",
    "WaitForSingleObject",
    "ExitProcess",
    "EnterCriticalSection",
    "LeaveCriticalSection",
    "InitializeCriticalSection",
    "CreateEventA",
    "SetEvent",
    "Sleep",
    "TlsAlloc",
    "TlsSetValue",
    "TlsGetValue",
    "GetCurrentThreadId",
    "GetExitCodeThread",
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
        a.e(&[0x6A, 0])
            .e(&[0x6A, 41])
            .push_label("worker")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
        a.e(&[0x89, 0xC6]); // mov esi, eax
        a.e(&[0x6A, 0xFF, 0x56]).call(l.iat_va(0, WAIT)); // WaitForSingleObject(h, INFINITE)
        a.e(&[0x68])
            .data_va(2)
            .e(&[0x56])
            .call(l.iat_va(0, GET_EXIT)); // GetExitCodeThread(h, &data[2])
        a.push_mem_data(2).call(l.iat_va(0, EXIT));
        a.label("worker");
        a.e(&[0x8B, 0x44, 0x24, 0x04, 0x40])
            .store(0)
            .e(&[0xC2, 4, 0]);
        a.finish()
    })
}

#[test]
fn a_thread_runs_and_a_waiter_gets_its_exit_code() {
    let (emu, stop) = run_with(&join_program(), 50, 100_000);
    assert_eq!(
        stop,
        Stop::Exit(42),
        "the exit code came back through WaitForSingleObject + GetExitCodeThread"
    );
    assert_eq!(data(&emu, 0), 42, "the thread ran with its own parameter");
    assert_eq!(emu.threads.len(), 2);
}

/// worker spins forever incrementing data[0]; main spins until data[0] >= 1000, then exits 7.
/// Both are busy loops: this only finishes if the scheduler preempts.
fn preemption_program() -> Vec<u8> {
    build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = Asm::new();
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0])
            .push_label("worker")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
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
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0])
            .push_label("work")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
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
    assert_eq!(
        emu.mem
            .read(crate::tests::util::data_va(8) + 12, 4)
            .unwrap(),
        0,
        "lock released at the end"
    );
}

#[test]
fn without_the_lock_updates_are_lost_so_the_test_above_has_teeth() {
    let (_, stop) = run_with(&counter_program(false), 3, 5_000_000);
    let Stop::Exit(total) = stop else {
        panic!("{stop:?}")
    };
    assert!(
        total < 2 * ROUNDS,
        "unsynchronized increments should race (got {total})"
    );
}

/// main: ev = CreateEvent(manual, unsignaled); t = CreateThread(waiter, ev); SetEvent(ev);
/// Wait(t); ExitProcess(data[0]). waiter: Wait(ev, INFINITE); data[0] = 5.
fn event_program() -> Vec<u8> {
    build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = Asm::new();
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 1, 0x6A, 0])
            .call(l.iat_va(0, CREATE_EVENT));
        a.e(&[0x89, 0xC6]); // esi = event
        a.e(&[0x6A, 0, 0x6A, 0, 0x56])
            .push_label("waiter")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
        a.e(&[0x89, 0xC7]); // edi = thread
        a.e(&[0x6A, 10]).call(l.iat_va(0, SLEEP)); // give the waiter time to block first
        a.e(&[0x56]).call(l.iat_va(0, SET_EVENT));
        a.e(&[0x6A, 0xFF, 0x57]).call(l.iat_va(0, WAIT));
        a.push_mem_data(0).call(l.iat_va(0, EXIT));
        a.label("waiter");
        a.e(&[0x6A, 0xFF, 0xFF, 0x74, 0x24, 0x08])
            .call(l.iat_va(0, WAIT)); // Wait(param, INFINITE)
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
    assert_eq!(
        emu.run(100_000),
        Stop::Exit(5),
        "SetEvent woke the waiter, which finished before main exited"
    );
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
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CREATE_EVENT));
        a.e(&[0x6A, 0xFF, 0x50]).call(l.iat_va(0, WAIT));
        a.finish()
    });
    let (_, stop) = run_with(&image, 100, 10_000);
    assert!(
        matches!(&stop, Stop::Unimplemented(m) if m.contains("deadlock")),
        "{stop:?}"
    );
}

#[test]
fn two_threads_waiting_on_each_other_are_a_deadlock_not_a_hang() {
    // main waits for the worker, the worker waits for an event nobody sets.
    let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = Asm::new();
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CREATE_EVENT));
        a.e(&[0x89, 0xC6]);
        a.e(&[0x6A, 0, 0x6A, 0, 0x56])
            .push_label("w")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
        a.e(&[0x6A, 0xFF, 0x50]).call(l.iat_va(0, WAIT));
        a.label("w");
        a.e(&[0x6A, 0xFF, 0xFF, 0x74, 0x24, 0x08])
            .call(l.iat_va(0, WAIT));
        a.e(&[0xC2, 4, 0]);
        a.finish()
    });
    let (_, stop) = run_with(&image, 20, 100_000);
    assert!(
        matches!(&stop, Stop::Unimplemented(m) if m.contains("deadlock")),
        "{stop:?}"
    );
}

/// TLS slots are per thread: main stores 111, the worker sees 0 and stores 222; main still reads 111.
fn tls_program() -> Vec<u8> {
    build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = Asm::new();
        a.call(l.iat_va(0, TLS_ALLOC)).store(5); // data[5] = slot
        a.e(&[0x6A, 111, 0x50]).call(l.iat_va(0, TLS_SET)); // TlsSetValue(slot, 111)
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0])
            .push_label("worker")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
        a.e(&[0x89, 0xC7]);
        a.e(&[0x6A, 0xFF, 0x57]).call(l.iat_va(0, WAIT));
        a.push_mem_data(5).call(l.iat_va(0, TLS_GET)); // main's value afterwards
        a.store(1);
        a.push_mem_data(0).call(l.iat_va(0, EXIT)); // exit(worker's initial value)
        a.label("worker");
        a.push_mem_data(5).call(l.iat_va(0, TLS_GET)).store(0); // data[0] = what the worker sees
        a.e(&[0x68, 222, 0, 0, 0])
            .e(&[0xFF, 0x35])
            .data_va(5)
            .call(l.iat_va(0, TLS_SET));
        a.e(&[0xC2, 4, 0]);
        a.finish()
    })
}

#[test]
fn thread_local_storage_is_per_thread() {
    let (emu, stop) = run_with(&tls_program(), 25, 100_000);
    assert_eq!(stop, Stop::Exit(0), "the worker started with an empty slot");
    assert_eq!(
        data(&emu, 1),
        111,
        "the worker's store did not touch main's value"
    );
}

#[test]
fn each_thread_has_its_own_id() {
    let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = Asm::new();
        a.call(l.iat_va(0, GET_TID)).store(1);
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0])
            .push_label("w")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
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
    let image = build_pe(
        &[("KERNEL32.dll", K32), ("user32.dll", &["GetMessageA"])],
        |l| {
            let mut a = Asm::new();
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0])
                .push_label("w")
                .e(&[0x6A, 0, 0x6A, 0])
                .call(l.iat_va(0, CT));
            a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0])
                .e(&[0x68])
                .data_va(16)
                .call(l.iat_va(1, 0)); // GetMessageA(&msg,0,0,0)
            a.e(&[0x6A, 0]).call(l.iat_va(0, EXIT));
            a.label("w");
            a.e(&[0xBE, 100, 0, 0, 0]);
            a.label("l")
                .e(&[0xFF, 0x05])
                .data_va(0)
                .e(&[0x4E])
                .jcc8(0x75, "l");
            a.e(&[0xC2, 4, 0]);
            a.finish()
        },
    );
    let (emu, stop) = run_with(&image, 20, 1_000_000);
    assert_eq!(
        stop,
        Stop::WaitForInput,
        "asks the host for input only once nothing else can run"
    );
    assert_eq!(
        data(&emu, 0),
        100,
        "the worker finished its work in the meantime"
    );
}

#[test]
fn suspended_threads_do_not_run_until_resumed() {
    // CREATE_SUSPENDED thread would set data[0]; main checks it stays 0 while suspended.
    let image = build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = Asm::new();
        a.e(&[0x6A, 0, 0x6A, 4, 0x6A, 0])
            .push_label("w")
            .e(&[0x6A, 0, 0x6A, 0])
            .call(l.iat_va(0, CT));
        a.e(&[0x6A, 50]).call(l.iat_va(0, SLEEP));
        a.push_mem_data(0).call(l.iat_va(0, EXIT));
        a.label("w");
        a.e(&[0xC7, 0x05])
            .data_va(0)
            .e(&1u32.to_le_bytes())
            .e(&[0xC2, 4, 0]);
        a.finish()
    });
    let mut emu = Emu::new();
    emu.win.virtual_now = Some(0);
    emu.quantum = 20;
    loader::load(&mut emu, &image).unwrap();
    assert_eq!(emu.run(100_000), Stop::Sleeping);
    emu.win.virtual_now = Some(50);
    assert_eq!(
        emu.run(100_000),
        Stop::Exit(0),
        "the suspended thread never ran"
    );
    let _ = CODE_VA;
}
