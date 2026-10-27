//! Tests of `kernel32_extra`.

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
