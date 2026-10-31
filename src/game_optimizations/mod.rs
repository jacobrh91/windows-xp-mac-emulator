//! Optimizations and set-up for particular games: which functions of their DLLs to replace
//! with native code (see `hooks`), replacements only one game needs, and what the game's
//! installer would have left on the machine. Each game has its own module; a new one adds
//! its hook table to `GAMES`, its stubs to `lookup`, its state to `GameState` and its
//! preparations to `setup`.
//!
//! Every hook is checked byte for byte against the DLL before it goes in, so one game's hooks
//! are harmless while another game (or another build of the same one) is running.

pub mod impossible_creatures;
pub mod rollercoaster_tycoon;

use crate::cpu::Emu;
use crate::hooks::Hook;
use crate::win32::Handler;

const GAMES: &[&[&[Hook]]] = &[impossible_creatures::TABLES];

/// Every game's hooks.
pub fn hooks() -> impl Iterator<Item = &'static Hook> {
    GAMES.iter().flat_map(|g| g.iter()).flat_map(|t| t.iter())
}

/// The native stub for a game hook's name.
pub fn lookup(name: &str) -> Option<(usize, Handler)> {
    impossible_creatures::lookup(name)
}

/// Prepare for running the program `exe` (its file name), before it starts.
pub fn setup(e: &mut Emu, exe: &str) {
    rollercoaster_tycoon::setup(e, exe);
}

/// Fullscreen: how many points below the top edge still count as the edge, for the program at
/// `path`.
pub fn top_scroll_band(path: &str) -> f64 {
    let exe = std::path::Path::new(path).file_name();
    let exe = exe.map_or("".into(), |f| f.to_string_lossy());
    impossible_creatures::top_scroll_band(&exe)
}

/// Called all the time while running: lets a game prepare for or stop speeding up.
pub fn speedup_tick(e: &mut Emu) {
    rollercoaster_tycoon::speedup_tick(e);
}

/// What the games' native stubs remember between calls.
#[derive(Default)]
pub struct GameState {
    pub impossible_creatures: impossible_creatures::State,
    pub rollercoaster_tycoon: rollercoaster_tycoon::State,
}
