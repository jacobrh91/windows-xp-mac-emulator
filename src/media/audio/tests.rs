//! Tests of `audio`.

use super::*;
use crate::win32::MAX_ARGS;

const CB: u32 = 0x1000; // guest callback
const COUNT: u32 = 0x2000; // callback invocations
const LAST_MSG: u32 = 0x2004;
const FMT: u32 = 0x3000;
const HDR: u32 = 0x4000;

fn call(e: &mut Emu, name: &str, args: &[u32]) -> u32 {
    let (_, h) = lookup("winmm", name).unwrap_or_else(|| panic!("{name}"));
    let mut full = [0u32; MAX_ARGS];
    full[..args.len()].copy_from_slice(args);
    h(e, &full).unwrap()
}

/// An emulator with a guest callback that counts calls and remembers the message id:
/// mov eax,[esp+8]; mov [LAST_MSG],eax; inc dword [COUNT]; ret 20
fn setup() -> Emu {
    let mut e = Emu::new();
    e.win.virtual_now = Some(0);
    e.mem.map(0x1000, 0x20000);
    let mut code = vec![0x8B, 0x44, 0x24, 0x08, 0xA3];
    code.extend_from_slice(&LAST_MSG.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0x05]);
    code.extend_from_slice(&COUNT.to_le_bytes());
    code.extend_from_slice(&[0xC2, 20, 0]);
    e.mem.write_bytes(CB, &code).unwrap();
    // 22.05 kHz, 16-bit stereo PCM: 88200 bytes per second
    for (off, v, n) in [
        (0u32, 1u32, 2u32),
        (2, 2, 2),
        (4, 22050, 4),
        (8, 88200, 4),
        (12, 4, 2),
        (14, 16, 2),
    ] {
        e.mem.write(FMT + off, n, v).unwrap();
    }
    e
}

/// Service the devices and run whatever callbacks that queued.
fn pump(e: &mut Emu) -> bool {
    let fired = e.pump_devices().unwrap();
    e.run_device_callbacks_inline().unwrap();
    fired
}

fn count(e: &Emu) -> u32 {
    e.mem.read(COUNT, 4).unwrap()
}

fn header(e: &mut Emu, at: u32, len: u32) {
    e.mem.write_bytes(at, &[0; 32]).unwrap();
    e.mem.write(at, 4, 0x5000).unwrap();
    e.mem.write(at + 4, 4, len).unwrap();
}

fn open(e: &mut Emu) -> u32 {
    assert_eq!(
        call(
            e,
            "waveOutOpen",
            &[0x2100, 0xFFFF_FFFF, FMT, CB, 77, CALLBACK_FUNCTION]
        ),
        0
    );
    e.mem.read(0x2100, 4).unwrap()
}

fn flags(e: &Emu, hdr: u32) -> u32 {
    e.mem.read(hdr + 16, 4).unwrap()
}

#[test]
fn open_reports_wom_open_and_close_reports_wom_close() {
    let mut e = setup();
    let h = open(&mut e);
    assert_ne!(h, 0);
    assert_eq!((count(&e), e.mem.read(LAST_MSG, 4).unwrap()), (1, WOM_OPEN));
    assert_eq!(call(&mut e, "waveOutClose", &[h]), 0);
    assert_eq!(
        (count(&e), e.mem.read(LAST_MSG, 4).unwrap()),
        (2, WOM_CLOSE)
    );
    assert_eq!(
        call(&mut e, "waveOutClose", &[h]),
        MMSYSERR_INVALHANDLE,
        "already closed"
    );
}

#[test]
fn unsupported_formats_are_refused_and_queries_open_nothing() {
    let mut e = setup();
    e.mem.write(FMT, 2, 3).unwrap(); // WAVE_FORMAT_IEEE_FLOAT
    assert_eq!(
        call(&mut e, "waveOutOpen", &[0x2100, 0xFFFF_FFFF, FMT, 0, 0, 0]),
        WAVERR_BADFORMAT
    );
    e.mem.write(FMT, 2, 1).unwrap();
    assert_eq!(
        call(
            &mut e,
            "waveOutOpen",
            &[0x2100, 0xFFFF_FFFF, FMT, 0, 0, WAVE_FORMAT_QUERY]
        ),
        0
    );
    assert!(
        e.win.audio.waves.is_empty(),
        "WAVE_FORMAT_QUERY must not open a device"
    );
}

#[test]
fn a_buffer_finishes_after_exactly_its_playing_time() {
    let mut e = setup();
    let h = open(&mut e);
    header(&mut e, HDR, 8820); // 8820 bytes at 88200 B/s = 100 ms
    assert_eq!(
        call(&mut e, "waveOutWrite", &[h, HDR, 32]),
        WAVERR_UNPREPARED,
        "must be prepared first"
    );
    assert_eq!(call(&mut e, "waveOutPrepareHeader", &[h, HDR, 32]), 0);
    assert_eq!(call(&mut e, "waveOutWrite", &[h, HDR, 32]), 0);
    assert_eq!(flags(&e, HDR) & (WHDR_INQUEUE | WHDR_DONE), WHDR_INQUEUE);

    e.win.virtual_now = Some(99);
    assert!(!pump(&mut e), "99 ms in: still playing");
    assert_eq!(flags(&e, HDR) & WHDR_DONE, 0);
    e.win.virtual_now = Some(100);
    assert!(pump(&mut e));
    assert_eq!(
        flags(&e, HDR) & (WHDR_INQUEUE | WHDR_DONE),
        WHDR_DONE,
        "done, no longer queued"
    );
    assert_eq!(
        (count(&e), e.mem.read(LAST_MSG, 4).unwrap()),
        (2, WOM_DONE),
        "WOM_OPEN then WOM_DONE"
    );
    assert_eq!(call(&mut e, "waveOutUnprepareHeader", &[h, HDR, 32]), 0);
}

#[test]
fn queued_buffers_play_back_to_back() {
    let mut e = setup();
    let h = open(&mut e);
    let h2 = HDR + 0x100;
    for hdr in [HDR, h2] {
        header(&mut e, hdr, 8820);
        call(&mut e, "waveOutPrepareHeader", &[h, hdr, 32]);
        call(&mut e, "waveOutWrite", &[h, hdr, 32]);
    }
    e.win.virtual_now = Some(100);
    pump(&mut e);
    assert_eq!(
        (flags(&e, HDR) & WHDR_DONE, flags(&e, h2) & WHDR_DONE),
        (WHDR_DONE, 0),
        "the second is still playing"
    );
    e.win.virtual_now = Some(200);
    pump(&mut e);
    assert_eq!(flags(&e, h2) & WHDR_DONE, WHDR_DONE);
}

#[test]
fn a_buffer_written_after_the_queue_drained_starts_from_now() {
    let mut e = setup();
    let h = open(&mut e);
    header(&mut e, HDR, 8820);
    call(&mut e, "waveOutPrepareHeader", &[h, HDR, 32]);
    call(&mut e, "waveOutWrite", &[h, HDR, 32]);
    e.win.virtual_now = Some(150);
    pump(&mut e);
    let h2 = HDR + 0x100;
    header(&mut e, h2, 8820);
    call(&mut e, "waveOutPrepareHeader", &[h, h2, 32]);
    call(&mut e, "waveOutWrite", &[h, h2, 32]); // device was idle from 100 to 150
    e.win.virtual_now = Some(249);
    pump(&mut e);
    assert_eq!(flags(&e, h2) & WHDR_DONE, 0);
    e.win.virtual_now = Some(250);
    pump(&mut e);
    assert_eq!(flags(&e, h2) & WHDR_DONE, WHDR_DONE);
}

#[test]
fn reset_completes_everything_immediately_and_close_waits_for_the_queue() {
    let mut e = setup();
    let h = open(&mut e);
    header(&mut e, HDR, 88200);
    call(&mut e, "waveOutPrepareHeader", &[h, HDR, 32]);
    call(&mut e, "waveOutWrite", &[h, HDR, 32]);
    assert_eq!(call(&mut e, "waveOutClose", &[h]), WAVERR_STILLPLAYING);
    assert_eq!(
        call(&mut e, "waveOutUnprepareHeader", &[h, HDR, 32]),
        WAVERR_STILLPLAYING
    );
    assert_eq!(call(&mut e, "waveOutReset", &[h]), 0);
    assert_eq!(flags(&e, HDR) & (WHDR_DONE | WHDR_INQUEUE), WHDR_DONE);
    assert_eq!(e.mem.read(LAST_MSG, 4).unwrap(), WOM_DONE);
    assert_eq!(call(&mut e, "waveOutClose", &[h]), 0);
}

#[test]
fn position_tracks_the_playing_buffer() {
    let mut e = setup();
    let h = open(&mut e);
    header(&mut e, HDR, 8820);
    call(&mut e, "waveOutPrepareHeader", &[h, HDR, 32]);
    call(&mut e, "waveOutWrite", &[h, HDR, 32]);
    e.win.virtual_now = Some(50);
    e.mem.write(0x2200, 4, 4).unwrap(); // ask for TIME_BYTES
    call(&mut e, "waveOutGetPosition", &[h, 0x2200, 8]);
    assert_eq!(
        e.mem.read(0x2204, 4).unwrap(),
        4410,
        "half of the 8820-byte buffer after 50 of 100 ms"
    );
    e.mem.write(0x2200, 4, 1).unwrap(); // TIME_MS
    call(&mut e, "waveOutGetPosition", &[h, 0x2200, 8]);
    assert_eq!(e.mem.read(0x2204, 4).unwrap(), 50);
    e.mem.write(0x2200, 4, 2).unwrap(); // TIME_SAMPLES (4 bytes per stereo 16-bit sample)
    call(&mut e, "waveOutGetPosition", &[h, 0x2200, 8]);
    assert_eq!(e.mem.read(0x2204, 4).unwrap(), 1102);
}

#[test]
fn pausing_stops_the_clock_for_queued_audio() {
    let mut e = setup();
    let h = open(&mut e);
    header(&mut e, HDR, 8820);
    call(&mut e, "waveOutPrepareHeader", &[h, HDR, 32]);
    call(&mut e, "waveOutWrite", &[h, HDR, 32]);
    e.win.virtual_now = Some(40);
    call(&mut e, "waveOutPause", &[h]);
    e.win.virtual_now = Some(500);
    assert!(
        !pump(&mut e),
        "paused: nothing completes however long it takes"
    );
    call(&mut e, "waveOutRestart", &[h]);
    e.win.virtual_now = Some(559);
    assert!(
        !pump(&mut e),
        "60 ms of the 100 ms remain after restart at 500"
    );
    e.win.virtual_now = Some(560);
    assert!(pump(&mut e));
}

#[test]
fn event_and_window_callbacks() {
    let mut e = setup();
    let ev = e.win.vfs.create_event(false, false);
    assert_eq!(
        call(
            &mut e,
            "waveOutOpen",
            &[0x2100, 0xFFFF_FFFF, FMT, ev, 0, CALLBACK_EVENT]
        ),
        0
    );
    let h = e.mem.read(0x2100, 4).unwrap();
    header(&mut e, HDR, 882);
    call(&mut e, "waveOutPrepareHeader", &[h, HDR, 32]);
    call(&mut e, "waveOutWrite", &[h, HDR, 32]);
    e.win.virtual_now = Some(10);
    pump(&mut e);
    assert!(
        matches!(
            e.win.vfs.get(ev),
            Some(crate::win32::vfs::Obj::Event { signaled: true, .. })
        ),
        "CALLBACK_EVENT sets the event"
    );

    assert_eq!(
        call(
            &mut e,
            "waveOutOpen",
            &[0x2100, 0xFFFF_FFFF, FMT, 0x10010, 0, CALLBACK_WINDOW]
        ),
        0
    );
    let h2 = e.mem.read(0x2100, 4).unwrap();
    e.wm.queue.clear();
    header(&mut e, HDR + 0x100, 882);
    call(&mut e, "waveOutPrepareHeader", &[h2, HDR + 0x100, 32]);
    call(&mut e, "waveOutWrite", &[h2, HDR + 0x100, 32]);
    e.win.virtual_now = Some(30);
    pump(&mut e);
    let m = e.wm.queue.pop_front().unwrap();
    assert_eq!(
        (m.hwnd, m.message, m.wparam, m.lparam),
        (0x10010, WOM_DONE, h2, HDR + 0x100),
        "MM_WOM_DONE posted to the window"
    );
}

#[test]
fn periodic_timers_fire_every_interval_until_killed() {
    let mut e = setup();
    let id = call(&mut e, "timeSetEvent", &[20, 1, CB, 5, TIME_PERIODIC]);
    assert_ne!(id, 0);
    e.win.virtual_now = Some(19);
    pump(&mut e);
    assert_eq!(count(&e), 0);
    e.win.virtual_now = Some(20);
    pump(&mut e);
    e.win.virtual_now = Some(40);
    pump(&mut e);
    assert_eq!(count(&e), 2);
    assert_eq!(call(&mut e, "timeKillEvent", &[id]), 0);
    e.win.virtual_now = Some(100);
    pump(&mut e);
    assert_eq!(count(&e), 2, "killed timers stay quiet");
    assert_eq!(
        call(&mut e, "timeKillEvent", &[id]),
        MMSYSERR_INVALPARAM,
        "unknown id"
    );
}

#[test]
fn one_shot_timers_fire_once_and_a_slow_host_does_not_cause_a_burst() {
    let mut e = setup();
    call(&mut e, "timeSetEvent", &[10, 1, CB, 0, 0]);
    e.win.virtual_now = Some(1000);
    pump(&mut e);
    e.win.virtual_now = Some(2000);
    pump(&mut e);
    assert_eq!(count(&e), 1);

    let mut e = setup();
    call(&mut e, "timeSetEvent", &[10, 1, CB, 0, TIME_PERIODIC]);
    e.win.virtual_now = Some(1000); // 100 periods missed
    pump(&mut e);
    assert_eq!(count(&e), 1, "missed periods are skipped, not replayed");
}

#[test]
fn timer_event_mode_sets_an_event() {
    let mut e = setup();
    let ev = e.win.vfs.create_event(false, false);
    call(
        &mut e,
        "timeSetEvent",
        &[5, 1, ev, 0, TIME_CALLBACK_EVENT_SET],
    );
    e.win.virtual_now = Some(5);
    assert!(pump(&mut e));
    assert!(matches!(
        e.win.vfs.get(ev),
        Some(crate::win32::vfs::Obj::Event { signaled: true, .. })
    ));
}

#[test]
fn device_callbacks_run_on_their_own_thread_and_leave_other_code_alone() {
    // The callback clobbers eax, ecx, edx and the flags; it runs on the callback thread,
    // so the main thread's registers stay exactly as they were.
    let mut e = setup();
    let evil = 0x1100;
    e.mem
        .write_bytes(
            evil,
            &[
                0xB8, 0xEF, 0xBE, 0xAD, 0xDE, 0xB9, 1, 2, 3, 4, 0xBA, 5, 6, 7, 8, 0x31, 0xDB, 0x39,
                0xDB, 0xFF, 0x05, 0, 0x20, 0, 0, 0xC2, 20, 0,
            ],
        )
        .unwrap();
    // The main thread spins: `jmp $`.
    e.mem.write_bytes(0x1200, &[0xEB, 0xFE]).unwrap();
    crate::loader::init_process(&mut e).unwrap();
    e.eip = 0x1200;
    call(&mut e, "timeSetEvent", &[1, 1, evil, 0, TIME_PERIODIC]);
    e.regs[crate::cpu::EAX] = 0x1111;
    e.regs[crate::cpu::ECX] = 0x2222;
    e.regs[crate::cpu::EDX] = 0x3333;
    e.eflags = 0x246;
    e.win.virtual_now = Some(5);
    assert!(e.pump_devices().unwrap(), "the timer fired");
    assert_eq!(count(&e), 0, "nothing ran inline");
    e.run(200_000);
    assert!(count(&e) >= 1, "the callback thread ran it");
    // Back on the main thread, its registers are untouched.
    while e.cur_tid() != crate::win32::threads::MAIN_THREAD_ID {
        e.rotate();
    }
    assert_eq!(
        (
            e.regs[crate::cpu::EAX],
            e.regs[crate::cpu::ECX],
            e.regs[crate::cpu::EDX]
        ),
        (0x1111, 0x2222, 0x3333)
    );
    assert_eq!(e.eflags & 0x8D5, 0x246 & 0x8D5);
    assert_eq!(e.eip, 0x1200);
}

#[test]
fn caps_and_counts() {
    let mut e = setup();
    assert_eq!(call(&mut e, "waveOutGetNumDevs", &[]), 1);
    assert_eq!(call(&mut e, "waveOutGetDevCapsA", &[0, 0x2300, 52]), 0);
    assert_eq!(e.mem.read_cstr(0x2308).unwrap(), "Silent Audio Device");
    assert_eq!(e.mem.read(0x2300 + 44, 2).unwrap(), 2, "stereo");
    let h = open(&mut e);
    call(&mut e, "waveOutSetVolume", &[h, 0x4000_4000]);
    call(&mut e, "waveOutGetVolume", &[h, 0x2400]);
    assert_eq!(e.mem.read(0x2400, 4).unwrap(), 0x4000_4000);
}
