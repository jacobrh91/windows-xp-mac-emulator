//! Tests of `other_dlls`.

use super::*;
use crate::win32::MAX_ARGS;

fn call(e: &mut Emu, dll: &str, name: &str, args: &[u32]) -> u32 {
    let (_, h) = lookup(dll, name).unwrap_or_else(|| panic!("{dll}!{name}"));
    let mut full = [0u32; MAX_ARGS];
    full[..args.len()].copy_from_slice(args);
    h(e, &full).unwrap()
}

fn fmt(e: &mut Emu, f: &str, args: &[u32]) -> String {
    e.mem.map(0x20000, 0x10000);
    e.mem
        .write_bytes(0x20000, format!("{f}\0").as_bytes())
        .unwrap();
    let mut it = args.iter().copied();
    c_format(e, 0x20000, &mut || it.next().unwrap_or(0)).unwrap()
}

#[test]
fn printf_integers_and_padding() {
    let mut e = Emu::new();
    assert_eq!(
        fmt(&mut e, "%d|%5d|%-5d|%05d|%+d", &[42, 42, 42, 42, 42]),
        "42|   42|42   |00042|+42"
    );
    assert_eq!(
        fmt(&mut e, "%d %i", &[(-7i32) as u32, (-7i32) as u32]),
        "-7 -7"
    );
    assert_eq!(
        fmt(&mut e, "%05d", &[(-42i32) as u32]),
        "-0042",
        "zero padding goes after the sign"
    );
    assert_eq!(fmt(&mut e, "%u", &[u32::MAX]), "4294967295");
    assert_eq!(
        fmt(&mut e, "%x %X %#x %08X", &[255, 255, 255, 0xBEEF]),
        "ff FF 0xff 0000BEEF"
    );
    assert_eq!(fmt(&mut e, "%.3d", &[5]), "005");
    assert_eq!(
        fmt(&mut e, "%hd", &[0x1_0001]),
        "1",
        "h truncates to 16 bits"
    );
    assert_eq!(fmt(&mut e, "100%%", &[]), "100%");
    assert_eq!(
        fmt(&mut e, "%*d", &[4, 9]),
        "   9",
        "width from an argument"
    );
}

#[test]
fn printf_strings_and_chars() {
    let mut e = Emu::new();
    e.mem.map(0x30000, 0x1000);
    e.mem.write_bytes(0x30000, b"world\0").unwrap();
    assert_eq!(fmt(&mut e, "hello %s!", &[0x30000]), "hello world!");
    assert_eq!(
        fmt(&mut e, "[%8s][%-8s][%.3s]", &[0x30000, 0x30000, 0x30000]),
        "[   world][world   ][wor]"
    );
    assert_eq!(fmt(&mut e, "%s", &[0]), "(null)");
    assert_eq!(fmt(&mut e, "%c%c", &[b'o' as u32, b'k' as u32]), "ok");
    assert_eq!(
        fmt(&mut e, "%q", &[]),
        "%q",
        "unknown conversions are left visible"
    );
}

#[test]
fn wsprintf_is_cdecl_and_writes_to_the_buffer() {
    let (argc, h) = lookup("user32", "wsprintfA").unwrap();
    assert!(argc & CDECL != 0);
    let mut e = Emu::new();
    e.mem.map(0x20000, 0x10000);
    e.mem.write_bytes(0x20100, b"v%d.%d\0").unwrap();
    let mut a = [0u32; MAX_ARGS];
    (a[0], a[1], a[2], a[3]) = (0x21000, 0x20100, 1, 2);
    assert_eq!(h(&mut e, &a).unwrap(), 4);
    assert_eq!(e.mem.read_cstr(0x21000).unwrap(), "v1.2");
}

#[test]
fn wvsprintf_reads_a_va_list() {
    let mut e = Emu::new();
    e.mem.map(0x20000, 0x10000);
    e.mem.write_bytes(0x20100, b"%d-%x\0").unwrap();
    e.mem.write(0x22000, 4, 12).unwrap();
    e.mem.write(0x22004, 4, 255).unwrap();
    assert_eq!(
        call(&mut e, "user32", "wvsprintfA", &[0x21000, 0x20100, 0x22000]),
        5
    );
    assert_eq!(e.mem.read_cstr(0x21000).unwrap(), "12-ff");
}

#[test]
fn winsock_reports_no_network_cleanly() {
    let mut e = Emu::new();
    e.mem.map(0x20000, 0x10000);
    assert_eq!(
        call(&mut e, "ws2_32", "#115", &[0x0202, 0x21000]),
        0,
        "WSAStartup succeeds"
    );
    assert_eq!(e.mem.read(0x21000, 2).unwrap(), 0x0202);
    assert_eq!(
        call(&mut e, "ws2_32", "#23", &[2, 2, 17]),
        u32::MAX,
        "socket() fails"
    );
    assert_eq!(
        call(&mut e, "ws2_32", "#111", &[]),
        WSAEAFNOSUPPORT,
        "WSAGetLastError says why"
    );
    assert_eq!(call(&mut e, "ws2_32", "#9", &[0x1234]), 0x3412, "htons");
    assert_eq!(
        call(&mut e, "ws2_32", "#8", &[0x1234_5678]),
        0x7856_3412,
        "htonl"
    );
    e.mem.write_bytes(0x20000, b"192.168.1.5\0").unwrap();
    assert_eq!(
        call(&mut e, "ws2_32", "#11", &[0x20000]),
        u32::from_le_bytes([192, 168, 1, 5])
    );
    let p = call(
        &mut e,
        "ws2_32",
        "#12",
        &[u32::from_le_bytes([10, 0, 0, 1])],
    );
    assert_eq!(e.mem.read_cstr(p).unwrap(), "10.0.0.1");
}

#[test]
fn audio_devices_are_reported_missing_not_present() {
    let mut e = Emu::new();
    assert_eq!(
        call(&mut e, "winmm", "midiOutOpen", &[0; 6]),
        MMSYSERR_NODRIVER,
        "no MIDI hardware"
    );
    assert_eq!(call(&mut e, "winmm", "mixerGetNumDevs", &[]), 0);
    assert_eq!(call(&mut e, "winmm", "auxGetNumDevs", &[]), 0);
    let t1 = call(&mut e, "winmm", "timeGetTime", &[]);
    assert!(t1 < 5_000, "milliseconds since start, not wall-clock epoch");
}

#[test]
fn ime_calls_behave_as_if_no_ime_is_installed() {
    let mut e = Emu::new();
    assert_eq!(
        call(&mut e, "imm32", "ImmGetContext", &[0x10010]),
        0,
        "no input context"
    );
    assert_eq!(call(&mut e, "imm32", "ImmIsIME", &[0]), 0);
    assert_eq!(call(&mut e, "imm32", "ImmReleaseContext", &[0x10010, 0]), 1);
    for name in [
        "ImmAssociateContext",
        "ImmGetOpenStatus",
        "ImmNotifyIME",
        "ImmGetCompositionStringA",
    ] {
        assert!(crate::win32::Win32::is_supported("imm32", name), "{name}");
    }
    assert!(crate::win32::Win32::is_supported(
        "kernel32",
        "IsProcessorFeaturePresent"
    ));
    assert_eq!(
        call(&mut e, "kernel32", "IsProcessorFeaturePresent", &[10]),
        0
    );
}

#[test]
fn uuid_round_trip() {
    let mut e = Emu::new();
    e.mem.map(0x20000, 0x10000);
    assert_eq!(call(&mut e, "rpcrt4", "UuidCreate", &[0x21000]), 0);
    assert_eq!(
        call(&mut e, "rpcrt4", "UuidToStringA", &[0x21000, 0x22000]),
        0
    );
    let s = e.mem.read_cstr(e.mem.read(0x22000, 4).unwrap()).unwrap();
    assert_eq!(s.len(), 36);
    e.mem
        .write_bytes(0x23000, format!("{s}\0").as_bytes())
        .unwrap();
    assert_eq!(
        call(&mut e, "rpcrt4", "UuidFromStringA", &[0x23000, 0x24000]),
        0
    );
    assert_eq!(
        call(&mut e, "rpcrt4", "UuidEqual", &[0x21000, 0x24000, 0]),
        1,
        "string round trip preserves the UUID"
    );
    e.mem.write_bytes(0x23000, b"nope\0").unwrap();
    assert_eq!(
        call(&mut e, "rpcrt4", "UuidFromStringA", &[0x23000, 0x24000]),
        1705
    );
}

#[test]
fn display_modes_and_char_helpers() {
    let mut e = Emu::new();
    e.mem.map(0x20000, 0x10000);
    assert_eq!(
        call(&mut e, "user32", "EnumDisplaySettingsA", &[0, 2, 0x21000]),
        1
    );
    assert_eq!(
        (
            e.mem.read(0x21000 + 108, 4).unwrap(),
            e.mem.read(0x21000 + 112, 4).unwrap()
        ),
        (1024, 768)
    );
    assert_eq!(
        call(&mut e, "user32", "EnumDisplaySettingsA", &[0, 99, 0x21000]),
        0,
        "ran out of modes"
    );
    e.mem.write_bytes(0x22000, b"abc\0").unwrap();
    call(&mut e, "user32", "CharUpperA", &[0x22000]);
    assert_eq!(e.mem.read_cstr(0x22000).unwrap(), "ABC");
    assert_eq!(
        call(&mut e, "user32", "CharUpperA", &[b'q' as u32]),
        b'Q' as u32,
        "single character form"
    );
    assert_eq!(call(&mut e, "user32", "CharNextA", &[0x22000]), 0x22001);
    assert_eq!(
        call(&mut e, "user32", "CharNextA", &[0x22003]),
        0x22003,
        "stops at NUL"
    );
}
