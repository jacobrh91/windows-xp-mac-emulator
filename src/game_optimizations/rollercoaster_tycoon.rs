//! RollerCoaster Tycoon (Chris Sawyer / Hasbro Interactive, 1999).
//!
//! The game finds its files through what its installer wrote to the registry: `Path`, the
//! install folder, and `SetupPath`, the CD (which holds `Data\CSG1.DAT` and friends). The
//! CD's files are copied into the game's folder, so both point there.

use crate::cpu::Emu;

const SETUP_KEY: &str =
    r"HKEY_LOCAL_MACHINE\Software\Fish Technology Group\RollerCoaster Tycoon Setup";

/// Prepare the machine as the installer would have, when `exe` is the game.
pub(super) fn setup(e: &mut Emu, exe: &str) {
    if !exe.eq_ignore_ascii_case("rct.exe") {
        return;
    }
    e.win.registry.set_str(SETUP_KEY, "Path", r"C:\Game");
    e.win.registry.set_str(SETUP_KEY, "SetupPath", r"C:\Game");
}
