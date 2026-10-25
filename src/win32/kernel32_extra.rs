//! The long tail of Win32 APIs: resources, strings, interlocked ops, system info,
//! a stubbed-out registry and file system, and so on.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::cpu::{Emu, Res, Stop};
use crate::win32::{Args, Handler};

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    let r: (usize, Handler) = match (dll, name) {
        // ---- resources ----
        ("kernel32", "FindResourceA") => (3, |e, a| find_resource_api(e, a[0], a[2], a[1], false)),
        ("kernel32", "FindResourceW") => (3, |e, a| find_resource_api(e, a[0], a[2], a[1], true)),
        ("kernel32", "FindResourceExA") => {
            (4, |e, a| find_resource_api(e, a[0], a[1], a[2], false))
        }
        ("kernel32", "FindResourceExW") => (4, |e, a| find_resource_api(e, a[0], a[1], a[2], true)),
        ("kernel32", "LoadResource") => (2, |e, a| {
            let base = module_base(e, a[0]);
            Ok(base.wrapping_add(e.mem.read(a[1], 4)?))
        }),
        ("kernel32", "LockResource") => (1, |_, a| Ok(a[0])),
        ("kernel32", "SizeofResource") => (2, |e, a| Ok(e.mem.read(a[1].wrapping_add(4), 4)?)),
        ("kernel32", "FreeResource") => (1, |_, _| Ok(0)),
        ("user32", "LoadStringA") => (4, user32_load_string),

        // ---- paths / process info ----
        ("kernel32", "GetWindowsDirectoryA") => (2, |e, a| put_str(e, a[0], a[1], r"C:\WINDOWS")),
        ("kernel32", "GetSystemDirectoryA") => {
            (2, |e, a| put_str(e, a[0], a[1], r"C:\WINDOWS\system32"))
        }
        ("kernel32", "GetTempPathA") => (2, |e, a| put_str(e, a[1], a[0], r"C:\Temp\")),
        ("kernel32", "IsDebuggerPresent") => (0, |_, _| Ok(0)),
        // Programs report their own errors here; surface them (also when not tracing).
        ("kernel32", "OutputDebugStringA") => (1, |e, a| {
            let text = e.mem.read_cstr(a[0])?;
            eprintln!("[debug] {}", text.trim_end());
            e.win.debug_output.push(text);
            Ok(0)
        }),
        ("kernel32", "SetErrorMode") => (1, |_, _| Ok(0)),
        ("kernel32", "DisableThreadLibraryCalls") => (1, |_, _| Ok(1)),
        ("kernel32", "TerminateProcess") => (2, |_, a| Err(Stop::Exit(a[1]))),
        ("kernel32", "GetSystemInfo") => (1, k32_get_system_info),
        ("kernel32", "GetSystemTimeAsFileTime") => (1, |e, a| {
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let ft = (t.as_secs() + 11_644_473_600) * 10_000_000 + t.subsec_nanos() as u64 / 100;
            e.mem.write(a[0], 4, ft as u32)?;
            e.mem.write(a[0] + 4, 4, (ft >> 32) as u32)?;
            Ok(0)
        }),
        ("kernel32", "GetLocalTime") | ("kernel32", "GetSystemTime") => (1, |e, a| {
            // 2005-01-01 (a Saturday) 12:00:00.000
            for (i, v) in [2005u32, 1, 6, 1, 12, 0, 0, 0].iter().enumerate() {
                e.mem.write(a[0] + 2 * i as u32, 2, *v)?;
            }
            Ok(0)
        }),

        // ---- memory ----
        ("kernel32", "GlobalAlloc") | ("kernel32", "LocalAlloc") => (2, |e, a| {
            let p = e.heap_alloc(a[1])?;
            if a[0] & 0x40 != 0 {
                e.mem.fill(p, a[1], 0)?; // GMEM_ZEROINIT / LMEM_ZEROINIT
            }
            Ok(p)
        }),
        // NULL on success, the handle back on failure.
        ("kernel32", "GlobalFree") | ("kernel32", "LocalFree") => (1, |e, a| {
            Ok(if a[0] == 0 || e.heap_free(a[0]) {
                0
            } else {
                a[0]
            })
        }),
        ("kernel32", "GlobalLock") => (1, |_, a| Ok(a[0])),
        ("kernel32", "GlobalUnlock") => (1, |_, _| Ok(0)),

        // ---- interlocked ----
        ("kernel32", "InterlockedIncrement") => {
            (1, |e, a| interlocked(e, a[0], |v| v.wrapping_add(1), true))
        }
        ("kernel32", "InterlockedDecrement") => {
            (1, |e, a| interlocked(e, a[0], |v| v.wrapping_sub(1), true))
        }
        ("kernel32", "InterlockedExchange") => (2, |e, a| {
            let new = a[1];
            interlocked(e, a[0], move |_| new, false)
        }),
        ("kernel32", "InterlockedExchangeAdd") => (2, |e, a| {
            let add = a[1];
            interlocked(e, a[0], move |v| v.wrapping_add(add), false)
        }),
        ("kernel32", "InterlockedCompareExchange") => (3, |e, a| {
            let (exch, comp) = (a[1], a[2]);
            interlocked(e, a[0], move |v| if v == comp { exch } else { v }, false)
        }),

        // ---- strings ----
        ("kernel32", "lstrcpyA") => (2, |e, a| {
            let s = e.mem.read_bytes_cstr(a[1])?;
            e.mem.write_bytes(a[0], &s)?;
            Ok(a[0])
        }),
        ("kernel32", "lstrcpynA") => (3, |e, a| {
            if a[2] > 0 {
                let mut s = e.mem.read_bytes_cstr(a[1])?;
                s.truncate(a[2] as usize - 1);
                s.push(0);
                e.mem.write_bytes(a[0], &s)?;
            }
            Ok(a[0])
        }),
        ("kernel32", "lstrcatA") => (2, |e, a| {
            let end = a[0] + e.mem.read_cstr(a[0])?.len() as u32;
            let s = e.mem.read_bytes_cstr(a[1])?;
            e.mem.write_bytes(end, &s)?;
            Ok(a[0])
        }),
        ("kernel32", "lstrcmpA") => (2, |e, a| lstrcmp(e, a[0], a[1], false)),
        ("kernel32", "lstrcmpiA") => (2, |e, a| lstrcmp(e, a[0], a[1], true)),
        ("kernel32", "MulDiv") => (3, |_, a| {
            let (x, y, z) = (a[0] as i32 as i64, a[1] as i32 as i64, a[2] as i32 as i64);
            Ok(if z == 0 {
                u32::MAX
            } else {
                ((x * y + z / 2) / z) as u32
            })
        }),
        ("kernel32", "MultiByteToWideChar") => (6, k32_multi_byte_to_wide),
        ("kernel32", "WideCharToMultiByte") => (8, k32_wide_to_multi_byte),

        // (The registry is in `registry`.)
        ("advapi32", "GetUserNameA") => (2, |e, a| {
            // GetUserNameA counts the terminator in the returned size.
            let r = crate::win32::kernel32::name_out(e, a[0], a[1], "Player")?;
            if r != 0 {
                e.mem.write(a[1], 4, 7)?;
            }
            Ok(r)
        }),
        ("kernel32", "GetThreadPriority") => (1, |_, _| Ok(0)),

        // ---- user32 odds and ends ----
        ("user32", "GetSystemMetrics") => (1, |e, a| {
            let (w, h) = e.win.ddraw.screen().unwrap_or((1024, 768));
            Ok(match a[0] {
                0 => w, // SM_CXSCREEN
                1 => h, // SM_CYSCREEN
                _ => 0,
            })
        }),
        ("user32", "GetDesktopWindow") => (0, |_, _| Ok(0x100)),
        _ => return None,
    };
    Some(r)
}

/// Copy `s` into a guest buffer of `size` bytes. Returns the length written (without NUL),
/// or the required size including NUL if it does not fit — like GetModuleFileName & co.
fn put_str(e: &mut Emu, buf: u32, size: u32, s: &str) -> Res<u32> {
    if (s.len() as u32) + 1 > size {
        return Ok(s.len() as u32 + 1);
    }
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    e.mem.write_bytes(buf, &bytes)?;
    Ok(s.len() as u32)
}

impl crate::memory::Memory {
    /// The bytes of a NUL-terminated string, including the terminator.
    pub fn read_bytes_cstr(&self, addr: u32) -> Result<Vec<u8>, crate::memory::Fault> {
        let mut out = Vec::new();
        for i in 0..0x10000u32 {
            let b = self.read_u8(addr.wrapping_add(i))?;
            out.push(b);
            if b == 0 {
                break;
            }
        }
        Ok(out)
    }
}

fn interlocked(e: &mut Emu, addr: u32, f: impl FnOnce(u32) -> u32, return_new: bool) -> Res<u32> {
    let old = e.mem.read(addr, 4)?;
    let new = f(old);
    e.mem.write(addr, 4, new)?;
    Ok(if return_new { new } else { old })
}

fn lstrcmp(e: &mut Emu, a: u32, b: u32, ignore_case: bool) -> Res<u32> {
    let (mut x, mut y) = (e.mem.read_cstr(a)?, e.mem.read_cstr(b)?);
    if ignore_case {
        x = x.to_lowercase();
        y = y.to_lowercase();
    }
    Ok(match x.cmp(&y) {
        std::cmp::Ordering::Less => u32::MAX,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

fn k32_get_system_info(e: &mut Emu, a: &Args) -> Res<u32> {
    let p = a[0];
    e.mem.write_bytes(p, &[0; 36])?;
    e.mem.write(p + 4, 4, 4096)?; // page size
    e.mem.write(p + 8, 4, 0x10000)?; // min app address
    e.mem.write(p + 12, 4, 0x7FFE_FFFF)?; // max app address
    e.mem.write(p + 16, 4, 1)?; // active processor mask
    e.mem.write(p + 20, 4, 1)?; // processors
    e.mem.write(p + 24, 4, 586)?; // PROCESSOR_INTEL_PENTIUM
    e.mem.write(p + 28, 4, 0x10000)?; // allocation granularity
    e.mem.write(p + 32, 2, 6)?; // processor level
    Ok(0)
}

fn k32_multi_byte_to_wide(e: &mut Emu, a: &Args) -> Res<u32> {
    let (src, srclen, dst, dstlen) = (a[2], a[3] as i32, a[4], a[5]);
    let bytes = if srclen == -1 {
        e.mem.read_bytes_cstr(src)?
    } else {
        e.mem.read_bytes(src, srclen as u32)?
    };
    if dstlen == 0 {
        return Ok(bytes.len() as u32);
    }
    if (dstlen as usize) < bytes.len() {
        e.win.last_error = 122; // ERROR_INSUFFICIENT_BUFFER
        return Ok(0);
    }
    for (i, b) in bytes.iter().enumerate() {
        e.mem.write(dst + 2 * i as u32, 2, *b as u32)?; // Latin-1 -> UTF-16
    }
    Ok(bytes.len() as u32)
}

fn k32_wide_to_multi_byte(e: &mut Emu, a: &Args) -> Res<u32> {
    let (src, srclen, dst, dstlen) = (a[2], a[3] as i32, a[4], a[5]);
    let mut chars = Vec::new();
    let mut i = 0;
    loop {
        if srclen != -1 && i >= srclen as u32 {
            break;
        }
        let c = e.mem.read(src + 2 * i, 2)?;
        chars.push(if c < 256 { c as u8 } else { b'?' });
        i += 1;
        if srclen == -1 && c == 0 {
            break;
        }
    }
    if dstlen == 0 {
        return Ok(chars.len() as u32);
    }
    if (dstlen as usize) < chars.len() {
        e.win.last_error = 122;
        return Ok(0);
    }
    e.mem.write_bytes(dst, &chars)?;
    Ok(chars.len() as u32)
}

// ---- resources ---------------------------------------------------------------------------

enum Key {
    Id(u32),
    Name(String),
    Any,
}

fn module_base(e: &Emu, hmod: u32) -> u32 {
    if hmod == 0 { e.win.image_base } else { hmod }
}

fn read_key(e: &Emu, v: u32, wide: bool) -> Res<Key> {
    if v < 0x1_0000 {
        return Ok(Key::Id(v));
    }
    let s = if wide {
        let mut u = Vec::new();
        for i in 0..0x1000 {
            let c = e.mem.read(v + 2 * i, 2)? as u16;
            if c == 0 {
                break;
            }
            u.push(c);
        }
        String::from_utf16_lossy(&u)
    } else {
        e.mem.read_cstr(v)?
    };
    Ok(match s.strip_prefix('#').and_then(|n| n.parse().ok()) {
        Some(id) => Key::Id(id),
        None => Key::Name(s),
    })
}

/// Look up `key` in the resource directory at `dir`; returns (is_subdirectory, offset).
fn dir_find(e: &Emu, root: u32, dir: u32, key: &Key) -> Res<Option<(bool, u32)>> {
    let d = root + dir;
    let count = e.mem.read(d + 12, 2)? + e.mem.read(d + 14, 2)?;
    for i in 0..count {
        let ent = d + 16 + 8 * i;
        let (name, off) = (e.mem.read(ent, 4)?, e.mem.read(ent + 4, 4)?);
        let hit = match key {
            Key::Any => true,
            Key::Id(k) => name & 0x8000_0000 == 0 && name == *k,
            Key::Name(s) => {
                name & 0x8000_0000 != 0 && {
                    let sp = root + (name & 0x7FFF_FFFF);
                    let len = e.mem.read(sp, 2)?;
                    let units: Vec<u16> = (0..len)
                        .map(|j| e.mem.read(sp + 2 + 2 * j, 2).map(|c| c as u16))
                        .collect::<Result<_, _>>()?;
                    String::from_utf16_lossy(&units).eq_ignore_ascii_case(s)
                }
            }
        };
        if hit {
            return Ok(Some((off & 0x8000_0000 != 0, off & 0x7FFF_FFFF)));
        }
    }
    Ok(None)
}

/// The raw bytes of resource (type id, name) in the module `hmod`.
pub(crate) fn resource_bytes(
    e: &Emu,
    hmod: u32,
    ty: u32,
    name: u32,
    wide: bool,
) -> Res<Option<Vec<u8>>> {
    let base = module_base(e, hmod);
    let key = read_key(e, name, wide)?;
    let Some(entry) = find_resource(e, base, &Key::Id(ty), &key)? else {
        return Ok(None);
    };
    let (rva, size) = (e.mem.read(entry, 4)?, e.mem.read(entry + 4, 4)?);
    Ok(Some(e.mem.read_bytes(base + rva, size)?))
}

/// Address of the IMAGE_RESOURCE_DATA_ENTRY for (type, name), reading the module's PE
/// headers straight out of guest memory.
fn find_resource(e: &Emu, base: u32, ty: &Key, name: &Key) -> Res<Option<u32>> {
    if e.mem.read(base, 2)? != 0x5A4D {
        return Ok(None);
    }
    let pe = base + e.mem.read(base + 0x3C, 4)?;
    let rsrc_rva = e.mem.read(pe + 24 + 112, 4)?;
    if rsrc_rva == 0 {
        return Ok(None);
    }
    let root = base + rsrc_rva;
    let Some((true, type_dir)) = dir_find(e, root, 0, ty)? else {
        return Ok(None);
    };
    let Some((true, name_dir)) = dir_find(e, root, type_dir, name)? else {
        return Ok(None);
    };
    let Some((false, entry)) = dir_find(e, root, name_dir, &Key::Any)? else {
        return Ok(None);
    };
    Ok(Some(root + entry))
}

fn find_resource_api(e: &mut Emu, hmod: u32, ty: u32, name: u32, wide: bool) -> Res<u32> {
    let (ty, name) = (read_key(e, ty, wide)?, read_key(e, name, wide)?);
    match find_resource(e, module_base(e, hmod), &ty, &name)? {
        Some(entry) => Ok(entry),
        None => {
            e.win.last_error = 1814; // ERROR_RESOURCE_NAME_NOT_FOUND
            Ok(0)
        }
    }
}

fn user32_load_string(e: &mut Emu, a: &Args) -> Res<u32> {
    load_string(e, a, false)
}

/// LoadStringA/W. `cch` counts characters of the output buffer in either flavour.
pub(crate) fn load_string(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let (hinst, id, buf, cch) = (a[0], a[1], a[2], a[3]);
    let base = module_base(e, hinst);
    // Strings live in blocks of 16 per RT_STRING resource named (id / 16) + 1.
    let Some(entry) = find_resource(e, base, &Key::Id(6), &Key::Id((id >> 4) + 1))? else {
        return Ok(0);
    };
    let mut p = base + e.mem.read(entry, 4)?;
    for _ in 0..(id & 15) {
        p += 2 + 2 * e.mem.read(p, 2)?;
    }
    let len = e.mem.read(p, 2)?;
    let n = len.min(cch.saturating_sub(1));
    for i in 0..n {
        let c = e.mem.read(p + 2 + 2 * i, 2)?;
        if wide {
            e.mem.write(buf + 2 * i, 2, c)?;
        } else {
            e.mem
                .write(buf + i, 1, if c < 256 { c } else { b'?' as u32 })?;
        }
    }
    if cch > 0 {
        e.mem.write(
            buf + if wide { 2 * n } else { n },
            if wide { 2 } else { 1 },
            0,
        )?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win32::Win32;

    const BASE: u32 = 0x0050_0000;

    /// Build a fake module in guest memory whose resource tree holds:
    ///   RT_RCDATA(10) / id 101 / lang 0x409  -> "DATA!"
    ///   RT_STRING(6)  / id 1   / lang 0x409  -> strings 0..15 (string 2 = "Hi")
    ///   "CUSTOM" / "Thing" / lang 0          -> 3 bytes
    fn module_with_resources() -> Emu {
        let mut e = Emu::new();
        e.mem.map(BASE, 0x4000);
        e.mem.write(BASE, 2, 0x5A4D).unwrap();
        e.mem.write(BASE + 0x3C, 4, 0x40).unwrap();
        e.mem.write(BASE + 0x40 + 24 + 112, 4, 0x1000).unwrap(); // resource dir RVA
        let root = BASE + 0x1000;

        let dir = |e: &mut Emu, at: u32, named: u16, ids: u16, entries: &[(u32, u32)]| {
            e.mem.write_bytes(root + at, &[0; 16]).unwrap();
            e.mem.write(root + at + 12, 2, named as u32).unwrap();
            e.mem.write(root + at + 14, 2, ids as u32).unwrap();
            for (i, (n, o)) in entries.iter().enumerate() {
                e.mem.write(root + at + 16 + 8 * i as u32, 4, *n).unwrap();
                e.mem.write(root + at + 20 + 8 * i as u32, 4, *o).unwrap();
            }
        };
        let sub = 0x8000_0000u32;
        // root: named "CUSTOM" (string at 0x300), ids 6 and 10
        dir(
            &mut e,
            0x00,
            1,
            2,
            &[(sub | 0x300, sub | 0x40), (6, sub | 0x60), (10, sub | 0x80)],
        );
        // string table type -> name 1 -> lang
        dir(&mut e, 0x60, 0, 1, &[(1, sub | 0xA0)]);
        dir(&mut e, 0xA0, 0, 1, &[(0x409, 0xE0)]); // data entry at 0xE0
        // rcdata -> name 101 -> lang
        dir(&mut e, 0x80, 0, 1, &[(101, sub | 0xC0)]);
        dir(&mut e, 0xC0, 0, 1, &[(0x409, 0x100)]); // data entry at 0x100
        // CUSTOM -> named "Thing" (string at 0x320) -> lang
        dir(&mut e, 0x40, 1, 0, &[(sub | 0x320, sub | 0x120)]);
        dir(&mut e, 0x120, 0, 1, &[(0, 0x140)]); // data entry at 0x140

        let put_str16 = |e: &mut Emu, at: u32, s: &str| {
            e.mem.write(root + at, 2, s.len() as u32).unwrap();
            for (i, c) in s.encode_utf16().enumerate() {
                e.mem
                    .write(root + at + 2 + 2 * i as u32, 2, c as u32)
                    .unwrap();
            }
        };
        put_str16(&mut e, 0x300, "CUSTOM");
        put_str16(&mut e, 0x320, "Thing");

        // payloads live at RVA 0x2000.. (module-relative)
        let entry = |e: &mut Emu, at: u32, rva: u32, size: u32| {
            e.mem.write(root + at, 4, rva).unwrap();
            e.mem.write(root + at + 4, 4, size).unwrap();
        };
        entry(&mut e, 0x100, 0x2000, 5);
        e.mem.write_bytes(BASE + 0x2000, b"DATA!").unwrap();
        entry(&mut e, 0x140, 0x2100, 3);
        e.mem.write_bytes(BASE + 0x2100, b"xyz").unwrap();
        // string block: 16 length-prefixed UTF-16 strings; #2 = "Hi"
        let mut p = BASE + 0x2200;
        for i in 0..16 {
            let s = if i == 2 { "Hi" } else { "" };
            e.mem.write(p, 2, s.len() as u32).unwrap();
            for (j, c) in s.encode_utf16().enumerate() {
                e.mem.write(p + 2 + 2 * j as u32, 2, c as u32).unwrap();
            }
            p += 2 + 2 * s.len() as u32;
        }
        entry(&mut e, 0xE0, 0x2200, p - (BASE + 0x2200));
        e
    }

    fn call(e: &mut Emu, name: &str, args: &[u32; 8]) -> u32 {
        let mut full = [0u32; 12];
        full[..8].copy_from_slice(args);
        let args = &full;
        let (_, h) = lookup("kernel32", name)
            .or_else(|| lookup("user32", name))
            .unwrap_or_else(|| panic!("{name} not found"));
        h(e, args).unwrap()
    }

    #[test]
    fn find_load_lock_sizeof_by_id() {
        let mut e = module_with_resources();
        let h = call(&mut e, "FindResourceA", &[BASE, 101, 10, 0, 0, 0, 0, 0]);
        assert_ne!(h, 0);
        let size = call(&mut e, "SizeofResource", &[BASE, h, 0, 0, 0, 0, 0, 0]);
        let data = call(&mut e, "LoadResource", &[BASE, h, 0, 0, 0, 0, 0, 0]);
        assert_eq!(size, 5);
        assert_eq!(e.mem.read_bytes(data, 5).unwrap(), b"DATA!");
    }

    #[test]
    fn find_by_name_is_case_insensitive() {
        let mut e = module_with_resources();
        e.mem.map(0x20000, 0x100);
        e.mem.write_bytes(0x20000, b"custom\0").unwrap();
        e.mem.write_bytes(0x20010, b"THING\0").unwrap();
        let h = call(
            &mut e,
            "FindResourceA",
            &[BASE, 0x20010, 0x20000, 0, 0, 0, 0, 0],
        );
        assert_ne!(h, 0);
        assert_eq!(
            call(&mut e, "SizeofResource", &[BASE, h, 0, 0, 0, 0, 0, 0]),
            3
        );
    }

    #[test]
    fn missing_resource_sets_last_error() {
        let mut e = module_with_resources();
        let h = call(&mut e, "FindResourceA", &[BASE, 999, 10, 0, 0, 0, 0, 0]);
        assert_eq!(h, 0);
        assert_eq!(e.win.last_error, 1814);
    }

    #[test]
    fn hmodule_zero_means_main_image() {
        let mut e = module_with_resources();
        e.win.image_base = BASE;
        assert_ne!(
            call(&mut e, "FindResourceA", &[0, 101, 10, 0, 0, 0, 0, 0]),
            0
        );
    }

    #[test]
    fn load_string_reads_string_table() {
        let mut e = module_with_resources();
        e.mem.map(0x9000, 0x100);
        let n = call(&mut e, "LoadStringA", &[BASE, 2, 0x9000, 64, 0, 0, 0, 0]);
        assert_eq!(n, 2);
        assert_eq!(e.mem.read_cstr(0x9000).unwrap(), "Hi");
    }

    #[test]
    fn interlocked_ops() {
        let mut e = Emu::new();
        e.mem.map(0x9000, 0x100);
        e.mem.write(0x9000, 4, 5).unwrap();
        assert_eq!(
            call(
                &mut e,
                "InterlockedIncrement",
                &[0x9000, 0, 0, 0, 0, 0, 0, 0]
            ),
            6
        );
        assert_eq!(
            call(
                &mut e,
                "InterlockedExchange",
                &[0x9000, 9, 0, 0, 0, 0, 0, 0]
            ),
            6
        );
        assert_eq!(
            call(
                &mut e,
                "InterlockedCompareExchange",
                &[0x9000, 1, 9, 0, 0, 0, 0, 0]
            ),
            9
        );
        assert_eq!(e.mem.read(0x9000, 4).unwrap(), 1);
        assert_eq!(
            call(
                &mut e,
                "InterlockedCompareExchange",
                &[0x9000, 7, 99, 0, 0, 0, 0, 0]
            ),
            1
        );
        assert_eq!(
            e.mem.read(0x9000, 4).unwrap(),
            1,
            "failed compare leaves value alone"
        );
    }

    #[test]
    fn string_helpers() {
        let mut e = Emu::new();
        e.mem.map(0x9000, 0x1000);
        e.mem.write_bytes(0x9000, b"abc\0").unwrap();
        e.mem.write_bytes(0x9100, b"ABD\0").unwrap();
        assert_eq!(
            call(&mut e, "lstrcmpiA", &[0x9000, 0x9100, 0, 0, 0, 0, 0, 0]),
            u32::MAX
        );
        assert_eq!(
            call(&mut e, "lstrcmpA", &[0x9000, 0x9000, 0, 0, 0, 0, 0, 0]),
            0
        );
        call(&mut e, "lstrcatA", &[0x9000, 0x9100, 0, 0, 0, 0, 0, 0]);
        assert_eq!(e.mem.read_cstr(0x9000).unwrap(), "abcABD");
        call(&mut e, "lstrcpynA", &[0x9200, 0x9000, 4, 0, 0, 0, 0, 0]);
        assert_eq!(e.mem.read_cstr(0x9200).unwrap(), "abc");
    }

    #[test]
    fn multibyte_wide_roundtrip() {
        let mut e = Emu::new();
        e.mem.map(0x9000, 0x1000);
        e.mem.write_bytes(0x9000, b"hello\0").unwrap();
        let need = call(
            &mut e,
            "MultiByteToWideChar",
            &[0, 0, 0x9000, u32::MAX, 0, 0, 0, 0],
        );
        assert_eq!(need, 6, "length including NUL");
        call(
            &mut e,
            "MultiByteToWideChar",
            &[0, 0, 0x9000, u32::MAX, 0x9100, 16, 0, 0],
        );
        assert_eq!(e.mem.read(0x9102, 2).unwrap(), b'e' as u32);
        let n = call(
            &mut e,
            "WideCharToMultiByte",
            &[0, 0, 0x9100, u32::MAX, 0x9200, 16, 0, 0],
        );
        assert_eq!(n, 6);
        assert_eq!(e.mem.read_cstr(0x9200).unwrap(), "hello");
        // too-small output buffer
        assert_eq!(
            call(
                &mut e,
                "MultiByteToWideChar",
                &[0, 0, 0x9000, u32::MAX, 0x9300, 2, 0, 0]
            ),
            0
        );
        assert_eq!(e.win.last_error, 122);
    }

    #[test]
    fn put_str_reports_required_size() {
        let mut e = Emu::new();
        e.mem.map(0x9000, 0x1000);
        assert_eq!(put_str(&mut e, 0x9000, 64, "abc").unwrap(), 3);
        assert_eq!(e.mem.read_cstr(0x9000).unwrap(), "abc");
        assert_eq!(put_str(&mut e, 0x9100, 2, "abc").unwrap(), 4);
    }

    #[test]
    fn registry_is_empty_and_files_are_missing() {
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x1000);
        e.mem.write_bytes(0x10000, b"Software\\Anything\0").unwrap();
        let (_, h) = crate::win32::lookup("advapi32", "RegOpenKeyExA").unwrap();
        let mut args = [0u32; crate::win32::MAX_ARGS];
        args[..5].copy_from_slice(&[0x8000_0002, 0x10000, 0, 0, 0x10100]);
        assert_eq!(h(&mut e, &args).unwrap(), 2);
        assert!(Win32::is_supported("kernel32", "CreateFileA"));
    }
}
