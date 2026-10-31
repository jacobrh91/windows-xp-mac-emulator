//! RollerCoaster Tycoon (Chris Sawyer / Hasbro Interactive, 1999).
//!
//! The game finds its files through what its installer wrote to the registry: `Path`, the
//! install folder, and `SetupPath`, the CD (which holds `Data\CSG1.DAT` and friends). The
//! CD's files are copied into the game's folder, so that folder is also shown as the CD in
//! drive D:. The game wants a real CD there: without `D:\Saved Games` under a CD-style short
//! name, it quits to the title screen as soon as a scenario starts.
//!
//! Speeding up: it does at most four ticks between redraws of the park, and redrawing a busy
//! park takes most of its time, so while speeding up it redraws only a few times a second.

use std::time::{Duration, Instant};

use crate::cpu::Emu;

/// The game's count of ticks.
const TICKS: u32 = 0x6E_3B84;
/// The function redrawing what changed, and its first instruction (7 bytes,
/// `cmp byte ptr [0x971EF0], 0`).
const REDRAW: u32 = 0x5E_1653;
const REDRAW_START: [u8; 7] = [0x80, 0x3D, 0xF0, 0x1E, 0x97, 0x00, 0x00];
/// How long between redraws while speeding up.
const REDRAW_EVERY: Duration = Duration::from_millis(66);

#[derive(Default)]
pub struct State {
    /// A guest byte: non-zero skips the redraw. Zero until the code is patched.
    skip: u32,
    last_redraw: Option<Instant>,
}

/// Send the redraw function through a stub that first checks the `skip` byte:
///     cmp byte ptr [skip], 0 / je on / ret / on: <the first instruction> / jmp <the rest>
fn install(e: &mut Emu) -> crate::cpu::Res<()> {
    let start = e.mem.read_bytes(REDRAW, 7);
    if !start.is_ok_and(|b| b == REDRAW_START) {
        return Ok(()); // a different build of the game
    }
    // The stub gets a page to itself, or writes near it would count as overwriting code; so
    // does the byte, which changes all the time.
    let block = e.heap_alloc(0x3000)?;
    let skip = block;
    let stub = (block + 0x1000) & !0xFFF;
    let mut code = vec![0x80, 0x3D];
    code.extend_from_slice(&skip.to_le_bytes());
    code.extend_from_slice(&[0x00, 0x74, 0x01, 0xC3]);
    code.extend_from_slice(&REDRAW_START);
    code.push(0xE9);
    let after = stub + code.len() as u32 + 4;
    code.extend_from_slice(&(REDRAW + 7).wrapping_sub(after).to_le_bytes());
    e.mem.write_bytes(stub, &code)?;
    let mut jump = vec![0xE9];
    jump.extend_from_slice(&stub.wrapping_sub(REDRAW + 5).to_le_bytes());
    jump.extend_from_slice(&[0x90, 0x90]);
    e.mem.write_bytes(REDRAW, &jump)?;
    e.flush_code_caches();
    e.win.games.rollercoaster_tycoon.skip = skip;
    Ok(())
}

/// While speeding up, skip redraws except every so often; otherwise skip none.
pub(super) fn speedup_tick(e: &mut Emu) {
    if e.win.tick_counter.is_none() {
        return;
    }
    if e.win.turbo && e.win.games.rollercoaster_tycoon.skip == 0 && install(e).is_err() {
        e.win.tick_counter = None;
        return;
    }
    let State { skip, last_redraw } = &mut e.win.games.rollercoaster_tycoon;
    if *skip == 0 {
        return;
    }
    let due = last_redraw.is_none_or(|t| t.elapsed() >= REDRAW_EVERY);
    if due && e.win.turbo {
        *last_redraw = Some(Instant::now());
    }
    let (at, value) = (*skip, (e.win.turbo && !due) as u32);
    let _ = e.mem.write(at, 1, value);
}

const SETUP_KEY: &str =
    r"HKEY_LOCAL_MACHINE\Software\Fish Technology Group\RollerCoaster Tycoon Setup";

/// Prepare the machine as the installer would have, when `exe` is the game.
pub(super) fn setup(e: &mut Emu, exe: &str) {
    if !exe.eq_ignore_ascii_case("rct.exe") {
        return;
    }
    // It waits for each frame by reading the clock, doing a tick each, 25 ms apart.
    e.win.tick_counter = Some((TICKS, 40.0));
    e.win.registry.set_str(SETUP_KEY, "Path", r"C:\Game");
    e.win.registry.set_str(SETUP_KEY, "SetupPath", r"D:\");
    if let Ok(dir) = e.win.vfs.resolve(r"C:\Game") {
        e.win.vfs.mount_cd('D', &dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redraw_goes_through_a_stub_and_is_skipped_only_while_speeding_up() {
        let mut e = Emu::new();
        e.mem.map(REDRAW & !0xFFF, 0x1000);
        e.mem.write_bytes(REDRAW, &REDRAW_START).unwrap();
        e.win.tick_counter = Some((TICKS, 40.0));
        speedup_tick(&mut e);
        assert_eq!(
            e.win.games.rollercoaster_tycoon.skip, 0,
            "patched on demand"
        );

        e.win.turbo = true;
        speedup_tick(&mut e); // patches, and redraws first
        let skip = e.win.games.rollercoaster_tycoon.skip;
        assert_eq!(e.mem.read(REDRAW, 1).unwrap(), 0xE9);
        assert_eq!(e.mem.read(skip, 1).unwrap(), 0);
        speedup_tick(&mut e);
        assert_eq!(e.mem.read(skip, 1).unwrap(), 1);

        e.win.turbo = false;
        speedup_tick(&mut e);
        assert_eq!(e.mem.read(skip, 1).unwrap(), 0);
    }

    #[test]
    fn a_different_build_is_left_alone() {
        let mut e = Emu::new();
        e.mem.map(REDRAW & !0xFFF, 0x1000);
        e.win.tick_counter = Some((TICKS, 40.0));
        e.win.turbo = true;
        speedup_tick(&mut e);
        assert_eq!(e.mem.read(REDRAW, 1).unwrap(), 0);
        assert_eq!(e.win.games.rollercoaster_tycoon.skip, 0);
    }
}
