//! Impossible Creatures (Relic, 2003).
//!
//! `Filesystem.dll` carries zlib 1.1.3 and inflates every compressed file in the game's
//! archives through it; under the interpreter that made loading take minutes, so those
//! functions go to `hooks::zlib`. The other modules replace hot loops (`mathbox`: vertex
//! transforms, run for every animated creature each frame; `miles`: the sound mixer's inner
//! loop) and fix a bug of the game itself (`fire`).

mod fire;
mod mathbox;
mod miles;

use crate::hooks::Hook;
use crate::win32::Handler;

/// zlib in `Filesystem.dll`.
const ZLIB_HOOKS: &[Hook] = &[
    Hook {
        module: "filesystem",
        rva: 0xbc1c,
        expect: &[
            0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0x6A, 0x0F, 0xFF, 0x74, 0x24, 0x10,
        ],
        name: "inflateInit_",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "filesystem",
        rva: 0xbc32,
        expect: &[
            0x53, 0x55, 0x56, 0x8B, 0x74, 0x24, 0x10, 0x33, 0xDB, 0x3B, 0xF3, 0x57,
        ],
        name: "inflate",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "filesystem",
        rva: 0xbb01,
        expect: &[
            0x56, 0x8B, 0x74, 0x24, 0x08, 0x85, 0xF6, 0x74, 0x2C, 0x8B, 0x46, 0x1C,
        ],
        name: "inflateEnd",
        points_to: None,
        on_patch: None,
    },
];

/// All of the game's hook tables.
pub(super) const TABLES: &[&[Hook]] = &[ZLIB_HOOKS, mathbox::HOOKS, miles::HOOKS, fire::HOOKS];

/// Fullscreen, the mouse this close to the top edge scrolls the map up, so scrolling still works
/// with the menu bar's trigger at the very edge.
pub(super) fn top_scroll_band(exe: &str) -> f64 {
    if exe.eq_ignore_ascii_case("ic.exe") {
        64.0
    } else {
        0.0
    }
}

/// What the game's native stubs remember between calls.
#[derive(Default)]
pub struct State {
    mathbox: mathbox::State,
    miles: miles::State,
    fire: fire::State,
}

pub(super) fn lookup(name: &str) -> Option<(usize, Handler)> {
    mathbox::lookup(name)
        .or_else(|| miles::lookup(name))
        .or_else(|| fire::lookup(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::Emu;
    use crate::hooks::apply;

    #[test]
    fn only_matching_code_is_patched() {
        let mut e = Emu::new();
        e.mem.map(0x40030000, 0x20000);
        assert_eq!(
            apply(&mut e, "filesystem", 0x40030000),
            0,
            "zeros do not look like zlib"
        );
        let h = &ZLIB_HOOKS[1];
        e.mem.write_bytes(0x40030000 + h.rva, h.expect).unwrap();
        assert_eq!(apply(&mut e, "filesystem", 0x40030000), 1);
        assert_eq!(
            e.mem.read_u8(0x40030000 + h.rva).unwrap(),
            0xE9,
            "now a jump to the stub"
        );
        assert_eq!(apply(&mut e, "otherdll", 0x40030000), 0);
    }
}
