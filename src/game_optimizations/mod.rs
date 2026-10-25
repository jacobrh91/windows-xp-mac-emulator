//! Optimizations for particular games: which functions of their DLLs to replace with native
//! code (see `hooks`), and replacements only one game needs. Each game has its own module;
//! a new one adds its hook table to `GAMES`, its stubs to `lookup` and its state to
//! `GameState`.
//!
//! Every hook is checked byte for byte against the DLL before it goes in, so one game's hooks
//! are harmless while another game (or another build of the same one) is running.

pub mod impossible_creatures;

use crate::hooks::Hook;
use crate::win32::Handler;

const GAMES: &[&[Hook]] = &[impossible_creatures::HOOKS];

/// Every game's hooks.
pub fn hooks() -> impl Iterator<Item = &'static Hook> {
    GAMES.iter().flat_map(|g| g.iter())
}

/// The native stub for a game hook's name.
pub fn lookup(name: &str) -> Option<(usize, Handler)> {
    impossible_creatures::lookup(name)
}

/// What the games' native stubs remember between calls.
#[derive(Default)]
pub struct GameState {
    pub impossible_creatures: impossible_creatures::State,
}
