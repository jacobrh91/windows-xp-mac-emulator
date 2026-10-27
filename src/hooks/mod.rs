//! Native replacements for hot functions compiled into a game's own DLLs.
//!
//! Some libraries are linked statically into a DLL, so there is no import to replace. When
//! such a DLL loads, its known function entry points are checked byte for byte and, if they
//! match, overwritten with a jump to an emulator stub. The stub does the same job in Rust, on
//! the same guest data structures.
//!
//! This module holds the machinery and the replacements any game can use: the C runtime's
//! string functions (`crt`), zlib's inflate (`zlib`) and the stream functions of Miles' MP3
//! codec plugin (`mssmp3.asi`, see `media::miles_mp3`). Which functions of a particular
//! game's DLLs to patch, and replacements only one game needs, live in `game_optimizations`.
//! `XP_NO_HOOKS=1` turns all hooks off; `XP_NO_HOOKS=mssmp3.asi` (say) just one module's.

mod crt;
pub mod zlib;

use crate::cpu::Emu;
use crate::media::miles_mp3 as mp3;
use crate::win32::Handler;

pub(crate) struct Hook {
    pub module: &'static str,
    pub rva: u32,
    /// The function's first bytes, so a different build of the DLL is left alone.
    pub expect: &'static [u8],
    pub name: &'static str,
    /// For code with absolute addresses in it: (rva of a dword in the code, rva it must point
    /// to). Confirms the DLL sits where its own addresses say, which the native version assumes.
    pub points_to: Option<(u32, u32)>,
    /// Called with the module's base once the hook is in (for code that addresses the
    /// module's globals).
    pub on_patch: Option<fn(&mut Emu, u32)>,
}

/// Hooks for libraries shared by many games, wherever their DLL is found.
const HOOKS: &[Hook] = &[
    // Miles' MP3 codec plugin: its ASI stream functions (see `media::miles_mp3`).
    Hook {
        module: "mssmp3.asi",
        rva: 0x1c90,
        expect: &[0x53, 0x56, 0x57, 0x68, 0x30, 0xAA, 0x00, 0x00],
        name: "ASI_stream_open",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "mssmp3.asi",
        rva: 0x1dd0,
        expect: &[0x51, 0x53, 0x55, 0x8B, 0x6C, 0x24, 0x18, 0x56],
        name: "ASI_stream_process",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "mssmp3.asi",
        rva: 0x1ec0,
        expect: &[0x8B, 0x54, 0x24, 0x04, 0x8B, 0x44, 0x24, 0x08],
        name: "ASI_stream_seek",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "mssmp3.asi",
        rva: 0x1da0,
        expect: &[0x56, 0x8B, 0x74, 0x24, 0x08, 0x8B, 0x86, 0x70],
        name: "ASI_stream_close",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "mssmp3.asi",
        rva: 0x1f30,
        expect: &[0x83, 0xEC, 0x48, 0xB8, 0x22, 0x56, 0x00, 0x00],
        name: "ASI_stream_attribute",
        points_to: None,
        on_patch: None,
    },
    Hook {
        module: "mssmp3.asi",
        rva: 0x21c0,
        expect: &[0x8B, 0x4C, 0x24, 0x08, 0x83, 0xC8, 0xFF, 0x83],
        name: "ASI_stream_set_preference",
        points_to: None,
        on_patch: None,
    },
];

#[derive(Default)]
pub struct HookState {
    /// Decompressors by guest `z_stream` address.
    pub(crate) inflaters: std::collections::HashMap<u32, flate2::Decompress>,
}

/// Patch the hooked functions of a just-loaded module. Returns how many were patched.
pub fn apply(emu: &mut Emu, module: &str, base: u32) -> usize {
    // XP_NO_HOOKS=1 turns every hook off; XP_NO_HOOKS=<module>[,<module>] just those modules'.
    let off = std::env::var("XP_NO_HOOKS").ok();
    if off.as_deref().is_some_and(|v| {
        v.is_empty() || v == "1" || v.split(',').any(|m| m.eq_ignore_ascii_case(module))
    }) {
        return 0;
    }
    let mut n = 0;
    for h in HOOKS
        .iter()
        .chain(crate::game_optimizations::hooks())
        .filter(|h| h.module == module)
    {
        let at = base + h.rva;
        if emu
            .mem
            .read_bytes(at, h.expect.len() as u32)
            .ok()
            .as_deref()
            != Some(h.expect)
        {
            continue;
        }
        if let Some((at_rva, target)) = h.points_to
            && emu.mem.read(base + at_rva, 4).ok() != Some(base + target)
        {
            continue;
        }
        if let Some(f) = h.on_patch {
            f(emu, base);
        }
        let stub = emu.win.resolve("__hook", h.name);
        // jmp rel32: the stub then sees the original caller's return address and arguments.
        let rel = stub.wrapping_sub(at + 5);
        if emu.mem.write(at, 1, 0xE9).is_ok() && emu.mem.write(at + 1, 4, rel).is_ok() {
            n += 1;
        }
    }
    // The C runtime's string and memory functions, found by their export names.
    if module == "msvcr70" {
        for &(export, stub) in crt::CRT_EXPORTS {
            let Some(at) = emu
                .win
                .module_by_handle(base)
                .and_then(|m| m.exports.as_ref())
                .and_then(|e| e.by_name.get(export))
                .copied()
            else {
                continue;
            };
            let stub = emu.win.resolve("__hook", stub);
            if emu.mem.write(at, 1, 0xE9).is_ok()
                && emu.mem.write(at + 1, 4, stub.wrapping_sub(at + 5)).is_ok()
            {
                n += 1;
            }
        }
    }
    if n > 0 && emu.win.trace {
        eprintln!("[hooks] {module}: {n} functions replaced");
    }
    n
}

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "__hook" {
        return None;
    }
    if name.starts_with("crt_") {
        return crt::lookup(name);
    }
    Some(match name {
        "ASI_stream_open" => (3, mp3::stream_open),
        "ASI_stream_process" => (3, mp3::stream_process),
        "ASI_stream_seek" => (2, mp3::stream_seek),
        "ASI_stream_close" => (1, mp3::stream_close),
        "ASI_stream_attribute" => (2, mp3::stream_attribute),
        "ASI_stream_set_preference" => (3, mp3::stream_set_preference),
        _ => return zlib::lookup(name).or_else(|| crate::game_optimizations::lookup(name)),
    })
}

/// Call a stub with its arguments (tests).
#[cfg(test)]
pub(crate) fn call(e: &mut Emu, h: Handler, args: &[u32]) -> u32 {
    let mut full = [0u32; crate::win32::MAX_ARGS];
    full[..args.len()].copy_from_slice(args);
    h(e, &full).unwrap()
}
