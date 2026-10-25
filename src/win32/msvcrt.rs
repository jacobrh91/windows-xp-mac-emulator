//! The slice of the Microsoft C runtime (msvcrt.dll) that program startup and simple
//! games need. These are cdecl, so every entry is marked `CDECL`.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::cpu::{Emu, Res, Stop, EAX};
use crate::win32::{Handler, CDECL};

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "msvcrt" {
        return None;
    }
    let r: (usize, Handler) = match name {
        "__set_app_type" | "__setusermatherr" | "_cexit" | "_c_exit" => (CDECL | 1, |_, _| Ok(0)),
        "_adjust_fdiv_noop" => (CDECL, |_, _| Ok(0)),
        "__p__fmode" => (CDECL, |e, _| Ok(data_addr(e, "_fmode")?)),
        "__p__commode" => (CDECL, |e, _| Ok(data_addr(e, "_commode")?)),
        "_controlfp" => (CDECL | 2, |_, _| Ok(0x0009_001F)),
        "_initterm" => (CDECL | 2, initterm),
        "__getmainargs" => (CDECL | 5, get_main_args),
        "exit" | "_exit" => (CDECL | 1, |_, a| Err(Stop::Exit(a[0]))),
        "_XcptFilter" => (CDECL | 2, |_, _| Ok(0)),
        "_except_handler3" => (CDECL | 4, |_, _| Ok(1)), // ExceptionContinueSearch
        "srand" => (CDECL | 1, |e, a| {
            e.win.rand_seed = a[0];
            Ok(0)
        }),
        "rand" => (CDECL, |e, _| {
            // The MSVC linear congruential generator.
            e.win.rand_seed = e.win.rand_seed.wrapping_mul(214013).wrapping_add(2531011);
            Ok((e.win.rand_seed >> 16) & 0x7fff)
        }),
        "time" => (CDECL | 1, |e, a| {
            let t = e.win.fixed_time.unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as u32);
            if a[0] != 0 {
                e.mem.write(a[0], 4, t)?;
            }
            Ok(t)
        }),
        _ => return None,
    };
    Some(r)
}

/// Exported *variables* are imported by address, not called, so they need real memory.
pub fn is_data_symbol(name: &str) -> bool {
    matches!(name, "_adjust_fdiv" | "_acmdln" | "_wcmdln" | "_commode" | "_fmode" | "__argc" | "__argv" | "_environ")
}

pub fn data_symbol(e: &mut Emu, dll: &str, name: &str) -> Res<Option<u32>> {
    if dll.to_ascii_lowercase().trim_end_matches(".dll") != "msvcrt" || !is_data_symbol(name) {
        return Ok(None);
    }
    data_addr(e, name).map(Some)
}

/// Address of the (lazily created) variable `name`.
fn data_addr(e: &mut Emu, name: &str) -> Res<u32> {
    if let Some(&a) = e.win.data_syms.get(name) {
        return Ok(a);
    }
    let addr = e.heap_alloc(16)?;
    let init = match name {
        "_acmdln" => command_line_a(e)?,
        "_wcmdln" => {
            let p = e.heap_alloc(64)?;
            e.mem.write_wstr(p, "game.exe")?;
            p
        }
        _ => 0,
    };
    e.mem.write(addr, 4, init)?;
    e.win.data_syms.insert(name.to_string(), addr);
    Ok(addr)
}

fn command_line_a(e: &mut Emu) -> Res<u32> {
    if e.win.cmdline == 0 {
        let p = e.heap_alloc(16)?;
        e.mem.write_bytes(p, b"game.exe\0")?;
        e.win.cmdline = p;
    }
    Ok(e.win.cmdline)
}

/// _initterm(begin, end): call every non-null function pointer in the table.
fn initterm(e: &mut Emu, a: &crate::win32::Args) -> Res<u32> {
    let (mut p, end) = (a[0], a[1]);
    while p < end {
        let f = e.mem.read(p, 4)?;
        if f != 0 {
            e.call_function(f, &[])?;
        }
        p += 4;
    }
    Ok(e.regs[EAX])
}

/// __getmainargs(&argc, &argv, &envp, doWildCard, &startupinfo)
fn get_main_args(e: &mut Emu, a: &crate::win32::Args) -> Res<u32> {
    let cmd = command_line_a(e)?;
    let argv = e.heap_alloc(16)?;
    e.mem.write(argv, 4, cmd)?;
    e.mem.write(argv + 4, 4, 0)?;
    let envp = e.heap_alloc(8)?;
    e.mem.write(envp, 4, 0)?;
    e.mem.write(a[0], 4, 1)?;
    e.mem.write(a[1], 4, argv)?;
    e.mem.write(a[2], 4, envp)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win32::MAX_ARGS;

    fn call(e: &mut Emu, name: &str, args: &[u32]) -> u32 {
        let (_, h) = lookup("msvcrt", name).unwrap_or_else(|| panic!("{name}"));
        let mut full = [0u32; MAX_ARGS];
        full[..args.len()].copy_from_slice(args);
        h(e, &full).unwrap()
    }

    #[test]
    fn rand_matches_msvc_sequence() {
        let mut e = Emu::new();
        call(&mut e, "srand", &[1]);
        // Well-known first values of MSVC's rand() with the default seed.
        let seq: Vec<u32> = (0..3).map(|_| call(&mut e, "rand", &[])).collect();
        assert_eq!(seq, [41, 18467, 6334]);
    }

    #[test]
    fn all_msvcrt_entries_are_cdecl() {
        for n in ["rand", "srand", "time", "exit", "_initterm", "__getmainargs", "_controlfp"] {
            assert!(lookup("msvcrt", n).unwrap().0 & CDECL != 0, "{n}");
        }
        assert!(lookup("kernel32", "rand").is_none());
    }

    #[test]
    fn data_symbols_are_stable_memory() {
        let mut e = Emu::new();
        let a = data_symbol(&mut e, "msvcrt.dll", "_acmdln").unwrap().unwrap();
        assert_eq!(a, data_symbol(&mut e, "MSVCRT.DLL", "_acmdln").unwrap().unwrap());
        let s = e.mem.read(a, 4).unwrap();
        assert_eq!(e.mem.read_cstr(s).unwrap(), "game.exe", "_acmdln holds a char*");
        assert_eq!(data_symbol(&mut e, "msvcrt", "memcpy").unwrap(), None);
        assert_eq!(data_symbol(&mut e, "kernel32", "_acmdln").unwrap(), None);
    }

    #[test]
    fn getmainargs_fills_argc_argv() {
        let mut e = Emu::new();
        e.mem.map(0x20000, 0x1000);
        call(&mut e, "__getmainargs", &[0x20000, 0x20004, 0x20008, 0, 0x2000C]);
        assert_eq!(e.mem.read(0x20000, 4).unwrap(), 1);
        let argv = e.mem.read(0x20004, 4).unwrap();
        assert_eq!(e.mem.read_cstr(e.mem.read(argv, 4).unwrap()).unwrap(), "game.exe");
    }

    #[test]
    fn time_writes_through_pointer() {
        let mut e = Emu::new();
        e.mem.map(0x20000, 0x1000);
        let t = call(&mut e, "time", &[0x20000]);
        assert_eq!(e.mem.read(0x20000, 4).unwrap(), t);
        assert!(t > 1_600_000_000);
    }

    #[test]
    fn cdecl_call_leaves_arguments_for_the_caller() {
        // push 5; call [srand]; add esp,4; hlt  -- if the callee popped too, esp would be off by 4
        let image = crate::tests::util::build_pe(&[("msvcrt.dll", &["srand"])], |l| {
            let mut c = vec![0x6A, 5, 0xFF, 0x15];
            c.extend_from_slice(&l.iat_va(0, 0).to_le_bytes());
            c.extend_from_slice(&[0x83, 0xC4, 4, 0xF4]);
            c
        });
        let mut e = Emu::new();
        crate::loader::load(&mut e, &image).unwrap();
        let esp_before = e.regs[crate::cpu::ESP];
        assert_eq!(e.run(100), Stop::Halt);
        assert_eq!(e.win.rand_seed, 5);
        assert_eq!(e.regs[crate::cpu::ESP], esp_before, "caller's `add esp,4` balances the push; the callee popped nothing");
    }
}
