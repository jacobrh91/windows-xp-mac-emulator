//! Tests of `kernel32`.

use super::*;
use crate::win32::MAX_ARGS;

const WAIT_TIMEOUT: u32 = 258;

fn call(e: &mut Emu, name: &str, args: &[u32]) -> u32 {
    // Through the full API table: some kernel32 calls live in other modules (threads).
    let (_, h) = crate::win32::lookup("kernel32", name).unwrap_or_else(|| panic!("{name}"));
    let mut full = [0u32; MAX_ARGS];
    full[..args.len()].copy_from_slice(args);
    h(e, &full).unwrap()
}

fn emu_with_game_dir(tag: &str) -> (Emu, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("xp_k32_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::write(root.join("Data/file.txt"), b"hello world").unwrap();
    std::fs::write(
        root.join("ic.ini"),
        "[global]\ngameVersion = 1.1\n; c\nlang = english\n[other]\nlang = french\n",
    )
    .unwrap();
    let mut e = Emu::new();
    e.win.vfs.mount(r"C:\Game", &root);
    e.win.vfs.cwd = r"C:\Game".into();
    e.mem.map(0x20000, 0x10000);
    (e, root)
}

fn put(e: &mut Emu, at: u32, s: &str) -> u32 {
    e.mem.write_bytes(at, format!("{s}\0").as_bytes()).unwrap();
    at
}

#[test]
fn create_read_seek_through_the_guest_api() {
    let (mut e, _) = emu_with_game_dir("rw");
    let path = put(&mut e, 0x20000, r"Data\FILE.TXT");
    let h = call(
        &mut e,
        "CreateFileA",
        &[path, 0x8000_0000, 1, 0, 3, 0x80, 0],
    );
    assert_ne!(h, INVALID_HANDLE);
    assert_eq!(call(&mut e, "GetFileSize", &[h, 0]), 11);
    assert_eq!(call(&mut e, "ReadFile", &[h, 0x21000, 5, 0x22000, 0]), 1);
    assert_eq!(e.mem.read_bytes(0x21000, 5).unwrap(), b"hello");
    assert_eq!(e.mem.read(0x22000, 4).unwrap(), 5);
    assert_eq!(call(&mut e, "SetFilePointer", &[h, 6, 0, 0]), 6);
    call(&mut e, "ReadFile", &[h, 0x21000, 50, 0x22000, 0]);
    assert_eq!(e.mem.read(0x22000, 4).unwrap(), 5, "short read at EOF");
    assert_eq!(call(&mut e, "CloseHandle", &[h]), 1);
}

#[test]
fn missing_file_sets_last_error() {
    let (mut e, _) = emu_with_game_dir("miss");
    let path = put(&mut e, 0x20000, "nope.bin");
    assert_eq!(
        call(
            &mut e,
            "CreateFileA",
            &[path, 0x8000_0000, 1, 0, 3, 0x80, 0]
        ),
        INVALID_HANDLE
    );
    assert_eq!(e.win.last_error, ERROR_FILE_NOT_FOUND);
    assert_eq!(call(&mut e, "GetFileAttributesA", &[path]), INFINITE);
    let outside = put(&mut e, 0x20100, r"C:\Windows\win.ini");
    assert_eq!(call(&mut e, "GetFileAttributesA", &[outside]), INFINITE);
    assert_eq!(
        e.win.last_error, ERROR_PATH_NOT_FOUND,
        "unmounted paths do not exist for the guest"
    );
}

#[test]
fn find_first_file_fills_win32_find_data() {
    let (mut e, _) = emu_with_game_dir("find");
    let pat = put(&mut e, 0x20000, r"Data\*.txt");
    let h = call(&mut e, "FindFirstFileA", &[pat, 0x21000]);
    assert_ne!(h, INVALID_HANDLE);
    assert_eq!(e.mem.read_cstr(0x21000 + 44).unwrap(), "file.txt");
    assert_eq!(e.mem.read(0x21000 + 32, 4).unwrap(), 11, "nFileSizeLow");
    assert_eq!(call(&mut e, "FindNextFileA", &[h, 0x21000]), 0);
    assert_eq!(e.win.last_error, ERROR_NO_MORE_FILES);
    assert_eq!(call(&mut e, "FindClose", &[h]), 1);
    let w = 0x23000;
    e.mem.write_wstr(w, r"Data\*").unwrap();
    let hw = call(&mut e, "FindFirstFileW", &[w, 0x24000]);
    assert_eq!(
        e.mem.read_wstr(0x24000 + 44).unwrap(),
        ".",
        "wide search lists . first"
    );
    call(&mut e, "FindClose", &[hw]);
}

#[test]
fn full_path_and_cwd() {
    let (mut e, _) = emu_with_game_dir("paths");
    let p = put(&mut e, 0x20000, r"Data\..\ic.ini");
    let n = call(&mut e, "GetFullPathNameA", &[p, 260, 0x21000, 0x22000]);
    assert_eq!(e.mem.read_cstr(0x21000).unwrap(), r"C:\Game\ic.ini");
    assert_eq!(n, 14);
    assert_eq!(
        e.mem.read_cstr(e.mem.read(0x22000, 4).unwrap()).unwrap(),
        "ic.ini",
        "lpFilePart"
    );
    assert_eq!(call(&mut e, "GetCurrentDirectoryA", &[260, 0x21000]), 7);
    assert_eq!(e.mem.read_cstr(0x21000).unwrap(), r"C:\Game");
    assert_eq!(
        call(&mut e, "GetCurrentDirectoryA", &[3, 0x21000]),
        8,
        "too small: required size incl. NUL"
    );
}

#[test]
fn ini_files_are_read_through_the_vfs() {
    let (mut e, _) = emu_with_game_dir("ini");
    let (app, key, def, file) = (
        put(&mut e, 0x20000, "GLOBAL"),
        put(&mut e, 0x20100, "Lang"),
        put(&mut e, 0x20200, "none"),
        put(&mut e, 0x20300, "ic.ini"),
    );
    let n = call(
        &mut e,
        "GetPrivateProfileStringA",
        &[app, key, def, 0x21000, 64, file],
    );
    assert_eq!(
        (n, e.mem.read_cstr(0x21000).unwrap().as_str()),
        (7, "english")
    );
    let other = put(&mut e, 0x20400, "other");
    call(
        &mut e,
        "GetPrivateProfileStringA",
        &[other, key, def, 0x21000, 64, file],
    );
    assert_eq!(
        e.mem.read_cstr(0x21000).unwrap(),
        "french",
        "sections are separate"
    );
    let missing = put(&mut e, 0x20500, "zzz");
    call(
        &mut e,
        "GetPrivateProfileStringA",
        &[app, missing, def, 0x21000, 64, file],
    );
    assert_eq!(
        e.mem.read_cstr(0x21000).unwrap(),
        "none",
        "default for a missing key"
    );
    call(
        &mut e,
        "GetPrivateProfileStringA",
        &[app, key, def, 0x21000, 4, file],
    );
    assert_eq!(
        e.mem.read_cstr(0x21000).unwrap(),
        "eng",
        "truncated to the buffer"
    );
    assert_eq!(ini_lookup("[a]\nx=1", "A", "X").as_deref(), Some("1"));
}

#[test]
fn file_mapping_copies_the_file_into_memory() {
    let (mut e, _) = emu_with_game_dir("map");
    let path = put(&mut e, 0x20000, r"Data\file.txt");
    let f = call(
        &mut e,
        "CreateFileA",
        &[path, 0x8000_0000, 1, 0, 3, 0x80, 0],
    );
    let m = call(&mut e, "CreateFileMappingA", &[f, 0, 2, 0, 0, 0]);
    assert_ne!(m, 0);
    let view = call(&mut e, "MapViewOfFile", &[m, 4, 0, 0, 0]);
    assert_eq!(e.mem.read_bytes(view, 11).unwrap(), b"hello world");
    let anon = call(
        &mut e,
        "CreateFileMappingA",
        &[INVALID_HANDLE, 0, 4, 0, 0x1000, 0],
    );
    let v2 = call(&mut e, "MapViewOfFile", &[anon, 6, 0, 0, 0]);
    assert_eq!(
        e.mem.read(v2, 4).unwrap(),
        0,
        "anonymous mappings are zeroed"
    );
}

#[test]
fn string_functions() {
    let (mut e, _) = emu_with_game_dir("str");
    let (a, b) = (put(&mut e, 0x20000, "Hello"), put(&mut e, 0x20100, "hello"));
    assert_eq!(
        call(
            &mut e,
            "CompareStringA",
            &[0x409, 0, a, u32::MAX, b, u32::MAX]
        ),
        1,
        "'H' < 'h'"
    );
    assert_eq!(
        call(
            &mut e,
            "CompareStringA",
            &[0x409, 1, a, u32::MAX, b, u32::MAX]
        ),
        2,
        "NORM_IGNORECASE"
    );
    assert_eq!(
        call(
            &mut e,
            "LCMapStringA",
            &[0x409, 0x200, a, u32::MAX, 0x21000, 16]
        ),
        6
    );
    assert_eq!(e.mem.read_cstr(0x21000).unwrap(), "HELLO");
    assert_eq!(
        call(&mut e, "LCMapStringA", &[0x409, 0x200, a, u32::MAX, 0, 0]),
        6,
        "size query"
    );
    e.mem.write_wstr(0x22000, "a1 ").unwrap();
    call(&mut e, "GetStringTypeW", &[1, 0x22000, 3, 0x23000]);
    let t = |i: u32| e.mem.read(0x23000 + 2 * i, 2).unwrap();
    assert!(t(0) & 0x102 == 0x102, "lowercase letter");
    assert!(t(1) & 0x4 != 0, "digit");
    assert!(t(2) & 0x8 != 0, "space");
    assert_eq!(call(&mut e, "GetLocaleInfoA", &[0x409, 0xE, 0x21000, 8]), 2);
    assert_eq!(e.mem.read_cstr(0x21000).unwrap(), ".");
}

#[test]
fn file_and_system_time_conversions_round_trip() {
    let (mut e, _) = emu_with_game_dir("time");
    // 2005-01-01 12:30:45.250 UTC
    for (i, v) in [2005u32, 1, 6, 1, 12, 30, 45, 250].iter().enumerate() {
        e.mem.write(0x20000 + 2 * i as u32, 2, *v).unwrap();
    }
    assert_eq!(call(&mut e, "SystemTimeToFileTime", &[0x20000, 0x21000]), 1);
    assert_eq!(call(&mut e, "FileTimeToSystemTime", &[0x21000, 0x22000]), 1);
    for i in 0..8 {
        assert_eq!(
            e.mem.read(0x22000 + 2 * i, 2).unwrap(),
            e.mem.read(0x20000 + 2 * i, 2).unwrap(),
            "field {i}"
        );
    }
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
}

#[test]
fn events_and_waiting() {
    let (mut e, _) = emu_with_game_dir("sync");
    let ev = call(&mut e, "CreateEventA", &[0, 0, 0, 0]); // auto-reset, unsignaled
    assert_eq!(call(&mut e, "WaitForSingleObject", &[ev, 0]), WAIT_TIMEOUT);
    call(&mut e, "SetEvent", &[ev]);
    assert_eq!(call(&mut e, "WaitForSingleObject", &[ev, 0]), 0);
    assert_eq!(
        call(&mut e, "WaitForSingleObject", &[ev, 0]),
        WAIT_TIMEOUT,
        "auto-reset consumed the signal"
    );
    let manual = call(&mut e, "CreateEventA", &[0, 1, 1, 0]);
    assert_eq!(call(&mut e, "WaitForSingleObject", &[manual, 0]), 0);
    assert_eq!(
        call(&mut e, "WaitForSingleObject", &[manual, 0]),
        0,
        "manual-reset stays signaled"
    );
    let (_, h) = crate::win32::lookup("kernel32", "WaitForSingleObject").unwrap();
    let mut args = [0u32; MAX_ARGS];
    args[0] = ev;
    args[1] = INFINITE;
    assert!(
        matches!(
            h(&mut e, &args),
            Err(Stop::Blocked(crate::win32::threads::Block::Forever))
        ),
        "an unsignaled infinite wait blocks; the scheduler decides whether that is a deadlock"
    );
    let mx = call(&mut e, "CreateMutexA", &[0, 0, 0]);
    assert_eq!(call(&mut e, "WaitForSingleObject", &[mx, 0]), 0);
    assert_eq!(call(&mut e, "ReleaseMutex", &[mx]), 1);
}

#[test]
fn environment_variables() {
    let (mut e, _) = emu_with_game_dir("env");
    let (name, val) = (
        put(&mut e, 0x20000, "Path"),
        put(&mut e, 0x20100, "C:\\Game"),
    );
    assert_eq!(
        call(&mut e, "GetEnvironmentVariableA", &[name, 0x21000, 64]),
        0
    );
    assert_eq!(e.win.last_error, 203);
    call(&mut e, "SetEnvironmentVariableA", &[name, val]);
    assert_eq!(
        call(&mut e, "GetEnvironmentVariableA", &[name, 0x21000, 64]),
        7
    );
    let block = call(&mut e, "GetEnvironmentStrings", &[]);
    assert_eq!(e.mem.read_cstr(block).unwrap(), "PATH=C:\\Game");
    call(&mut e, "SetEnvironmentVariableA", &[name, 0]);
    assert_eq!(
        call(&mut e, "GetEnvironmentVariableA", &[name, 0x21000, 64]),
        0
    );
}

#[test]
fn virtual_query_reports_committed_and_free() {
    let (mut e, _) = emu_with_game_dir("vq");
    assert_eq!(
        call(&mut e, "VirtualQuery", &[0x20000, 0x30000 - 0x1000, 28]),
        28
    );
    let (state, size) = (
        e.mem.read(0x30000 - 0x1000 + 16, 4).unwrap(),
        e.mem.read(0x30000 - 0x1000 + 12, 4).unwrap(),
    );
    assert_eq!((state, size), (0x1000, 0x10000), "the whole mapped region");
    call(&mut e, "VirtualQuery", &[0x1000_0000, 0x21000, 28]);
    assert_eq!(e.mem.read(0x21000 + 16, 4).unwrap(), 0x10000, "MEM_FREE");
    assert_eq!(call(&mut e, "IsBadReadPtr", &[0x20000, 16]), 0);
    assert_eq!(call(&mut e, "IsBadReadPtr", &[0x1000_0000, 16]), 1);
}
