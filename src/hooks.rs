//! Native replacements for hot functions compiled into the game's own DLLs.
//!
//! Some libraries are linked statically into a game DLL, so there is no import to replace.
//! When such a DLL loads, its known function entry points are checked byte for byte and, if
//! they match, overwritten with a jump to an emulator stub. The stub does the same job in
//! Rust, on the same guest data structures.
//!
//! Impossible Creatures' `Filesystem.dll` carries zlib 1.1.3 and inflates every compressed
//! file in the game's archives through it; under the interpreter that made loading take
//! minutes. The replacement uses the Mac's own zlib, which behaves exactly like the game's copy. `XP_NO_HOOKS=1` turns all hooks off.

use std::collections::HashMap;

use flate2::{Decompress, FlushDecompress, Status};

use crate::cpu::{Emu, Res};
use crate::win32::{Args, Handler};

struct Hook {
    module: &'static str,
    rva: u32,
    /// The function's first bytes, so a different build of the DLL is left alone.
    expect: &'static [u8],
    name: &'static str,
}

const HOOKS: &[Hook] = &[
    Hook { module: "filesystem", rva: 0xbc1c, expect: &[0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0x6A, 0x0F, 0xFF, 0x74, 0x24, 0x10], name: "inflateInit_" },
    Hook { module: "filesystem", rva: 0xbc32, expect: &[0x53, 0x55, 0x56, 0x8B, 0x74, 0x24, 0x10, 0x33, 0xDB, 0x3B, 0xF3, 0x57], name: "inflate" },
    Hook { module: "filesystem", rva: 0xbb01, expect: &[0x56, 0x8B, 0x74, 0x24, 0x08, 0x85, 0xF6, 0x74, 0x2C, 0x8B, 0x46, 0x1C], name: "inflateEnd" },
];

#[derive(Default)]
pub struct HookState {
    /// Decompressors by guest `z_stream` address.
    inflaters: HashMap<u32, Decompress>,
}

/// Patch the hooked functions of a just-loaded module. Returns how many were patched.
pub fn apply(emu: &mut Emu, module: &str, base: u32) -> usize {
    if std::env::var_os("XP_NO_HOOKS").is_some() {
        return 0;
    }
    let mut n = 0;
    for h in HOOKS.iter().filter(|h| h.module == module) {
        let at = base + h.rva;
        if emu.mem.read_bytes(at, h.expect.len() as u32).ok().as_deref() != Some(h.expect) {
            continue;
        }
        let stub = emu.win.resolve("__hook", h.name);
        // jmp rel32: the stub then sees the original caller's return address and arguments.
        let rel = stub.wrapping_sub(at + 5);
        if emu.mem.write(at, 1, 0xE9).is_ok() && emu.mem.write(at + 1, 4, rel).is_ok() {
            n += 1;
        }
    }
    if n > 0 && emu.win.trace {
        eprintln!("[hooks] {module}: {n} functions replaced");
    }
    n
}

// z_stream (zlib 1.1.3) field offsets.
const NEXT_IN: u32 = 0;
const AVAIL_IN: u32 = 4;
const TOTAL_IN: u32 = 8;
const NEXT_OUT: u32 = 12;
const AVAIL_OUT: u32 = 16;
const TOTAL_OUT: u32 = 20;
const MSG: u32 = 24;
const STATE: u32 = 28;

const Z_OK: u32 = 0;
const Z_STREAM_END: u32 = 1;
const Z_NEED_DICT: u32 = 2;
const Z_STREAM_ERROR: u32 = -2i32 as u32;
const Z_DATA_ERROR: u32 = -3i32 as u32;
const Z_BUF_ERROR: u32 = -5i32 as u32;
const Z_FINISH: u32 = 4;

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "__hook" {
        return None;
    }
    Some(match name {
        "inflateInit_" => (3, inflate_init),
        "inflate" => (2, inflate_call),
        "inflateEnd" => (1, inflate_end),
        _ => return None,
    })
}

fn inflate_init(e: &mut Emu, a: &Args) -> Res<u32> {
    let strm = a[0];
    if strm == 0 {
        return Ok(Z_STREAM_ERROR);
    }
    for off in [TOTAL_IN, TOTAL_OUT, MSG] {
        e.mem.write(strm + off, 4, 0)?;
    }
    // zlib checks `state` for null; any non-null marker will do (the stream itself).
    e.mem.write(strm + STATE, 4, strm)?;
    e.win.hooks.inflaters.insert(strm, Decompress::new(true));
    Ok(Z_OK)
}

fn inflate_call(e: &mut Emu, a: &Args) -> Res<u32> {
    let (strm, flush) = (a[0], a[1]);
    let r = |e: &Emu, off: u32| e.mem.read(strm + off, 4);
    let (next_in, avail_in, next_out, avail_out) = (r(e, NEXT_IN)?, r(e, AVAIL_IN)?, r(e, NEXT_OUT)?, r(e, AVAIL_OUT)?);
    let input = e.mem.read_bytes(next_in, avail_in)?;
    let mut out = vec![0u8; avail_out as usize];
    let Some(z) = e.win.hooks.inflaters.get_mut(&strm) else { return Ok(Z_STREAM_ERROR) };
    // The host's zlib, so consumption and return codes match the game's own copy exactly.
    let (in0, out0) = (z.total_in(), z.total_out());
    let mode = match flush {
        Z_FINISH => FlushDecompress::Finish,
        0 => FlushDecompress::None,
        _ => FlushDecompress::Sync,
    };
    let result = z.decompress(&input, &mut out, mode);
    let (used, made) = ((z.total_in() - in0) as u32, (z.total_out() - out0) as u32);
    let status = match result {
        Ok(Status::Ok) => Z_OK,
        Ok(Status::StreamEnd) => Z_STREAM_END,
        Ok(Status::BufError) => Z_BUF_ERROR,
        Err(ref err) if err.needs_dictionary().is_some() => Z_NEED_DICT,
        Err(_) => Z_DATA_ERROR,
    };
    if e.win.trace {
        eprintln!("[hooks] inflate {strm:#x}: in {avail_in} out {avail_out} -> used {used} made {made} {status:?}");
    }
    e.mem.write_bytes(next_out, &out[..made as usize])?;
    e.mem.write(strm + NEXT_IN, 4, next_in + used)?;
    e.mem.write(strm + AVAIL_IN, 4, avail_in - used)?;
    e.mem.write(strm + TOTAL_IN, 4, r(e, TOTAL_IN)?.wrapping_add(used))?;
    e.mem.write(strm + NEXT_OUT, 4, next_out + made)?;
    e.mem.write(strm + AVAIL_OUT, 4, avail_out - made)?;
    e.mem.write(strm + TOTAL_OUT, 4, r(e, TOTAL_OUT)?.wrapping_add(made))?;
    Ok(status)
}

fn inflate_end(e: &mut Emu, a: &Args) -> Res<u32> {
    if e.win.hooks.inflaters.remove(&a[0]).is_none() {
        return Ok(Z_STREAM_ERROR);
    }
    e.mem.write(a[0] + STATE, 4, 0)?;
    Ok(Z_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win32::MAX_ARGS;

    fn compress(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data).unwrap();
        z.finish().unwrap()
    }

    fn call(e: &mut Emu, h: Handler, args: &[u32]) -> u32 {
        let mut full = [0u32; MAX_ARGS];
        full[..args.len()].copy_from_slice(args);
        h(e, &full).unwrap()
    }

    #[test]
    fn inflates_a_guest_stream_in_pieces() {
        let text: Vec<u8> = (0..20_000u32).map(|i| b"impossible creatures "[(i % 21) as usize]).collect();
        let packed = compress(&text);
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x40000);
        let (strm, src, dst) = (0x10000, 0x11000, 0x20000);
        e.mem.write_bytes(src, &packed).unwrap();
        assert_eq!(call(&mut e, inflate_init, &[strm, 0, 56]), Z_OK);
        assert_ne!(e.mem.read(strm + STATE, 4).unwrap(), 0);
        e.mem.write(strm + NEXT_IN, 4, src).unwrap();
        e.mem.write(strm + AVAIL_IN, 4, packed.len() as u32).unwrap();
        // Output in 4 KiB pieces, as a streaming reader would ask for it.
        let mut status = Z_OK;
        let mut out_at = dst;
        while status == Z_OK {
            e.mem.write(strm + NEXT_OUT, 4, out_at).unwrap();
            e.mem.write(strm + AVAIL_OUT, 4, 4096).unwrap();
            status = call(&mut e, inflate_call, &[strm, 2]);
            out_at = e.mem.read(strm + NEXT_OUT, 4).unwrap();
        }
        assert_eq!(status, Z_STREAM_END);
        assert_eq!(e.mem.read(strm + TOTAL_OUT, 4).unwrap(), text.len() as u32);
        assert_eq!(e.mem.read(strm + TOTAL_IN, 4).unwrap(), packed.len() as u32);
        assert_eq!(e.mem.read_bytes(dst, text.len() as u32).unwrap(), text);
        assert_eq!(call(&mut e, inflate_end, &[strm]), Z_OK);
        assert_eq!(call(&mut e, inflate_end, &[strm]), Z_STREAM_ERROR, "already ended");
    }

    #[test]
    fn an_exactly_sized_output_buffer_still_reports_the_end_of_the_stream() {
        let text = vec![7u8; 5000];
        let packed = compress(&text);
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x20000);
        let (strm, src, dst) = (0x10000, 0x11000, 0x18000);
        e.mem.write_bytes(src, &packed).unwrap();
        call(&mut e, inflate_init, &[strm, 0, 56]);
        e.mem.write(strm + NEXT_IN, 4, src).unwrap();
        e.mem.write(strm + AVAIL_IN, 4, packed.len() as u32).unwrap();
        e.mem.write(strm + NEXT_OUT, 4, dst).unwrap();
        e.mem.write(strm + AVAIL_OUT, 4, 5000).unwrap();
        assert_eq!(call(&mut e, inflate_call, &[strm, 2]), Z_STREAM_END, "as zlib does");
        assert_eq!(e.mem.read(strm + AVAIL_IN, 4).unwrap(), 0, "the checksum was consumed");
    }

    #[test]
    fn corrupt_data_is_reported() {
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x20000);
        let (strm, src, dst) = (0x10000, 0x11000, 0x12000);
        e.mem.write_bytes(src, &[0x78, 0x9C, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        call(&mut e, inflate_init, &[strm, 0, 56]);
        e.mem.write(strm + NEXT_IN, 4, src).unwrap();
        e.mem.write(strm + AVAIL_IN, 4, 6).unwrap();
        e.mem.write(strm + NEXT_OUT, 4, dst).unwrap();
        e.mem.write(strm + AVAIL_OUT, 4, 100).unwrap();
        assert_eq!(call(&mut e, inflate_call, &[strm, 0]), Z_DATA_ERROR);
    }

    #[test]
    fn only_matching_code_is_patched() {
        let mut e = Emu::new();
        e.mem.map(0x40030000, 0x20000);
        assert_eq!(apply(&mut e, "filesystem", 0x40030000), 0, "zeros do not look like zlib");
        let h = &HOOKS[1];
        e.mem.write_bytes(0x40030000 + h.rva, h.expect).unwrap();
        assert_eq!(apply(&mut e, "filesystem", 0x40030000), 1);
        assert_eq!(e.mem.read_u8(0x40030000 + h.rva).unwrap(), 0xE9, "now a jump to the stub");
        assert_eq!(apply(&mut e, "otherdll", 0x40030000), 0);
    }
}
