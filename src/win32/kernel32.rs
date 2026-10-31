//! kernel32 on top of the sandboxed `Vfs`: files, searches, mappings, paths, plus the locale,
//! environment, INI, event/mutex and many small calls the C runtime and engines expect.

use crate::cpu::{Emu, Res, Stop};
use crate::win32::vfs::*;
use crate::win32::{Args, Handler};

const PROCESS_HEAP: u32 = 0x1EAB_0000;
const MAIN_THREAD: u32 = 0x500;
const INFINITE: u32 = 0xFFFF_FFFF;

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    // (Threads, locks, TLS, waiting and sleeping live in `threads`.)
    if dll != "kernel32" {
        return None;
    }
    let r: (usize, Handler) = match name {
        // ---- files ----
        "CreateFileA" => (7, |e, a| create_file(e, a, false)),
        "CreateFileW" => (7, |e, a| create_file(e, a, true)),
        "ReadFile" => (5, read_file),
        "SetFilePointer" => (4, set_file_pointer),
        "GetFileSize" => (2, |e, a| match e.win.vfs.file_size(a[0]) {
            Ok(sz) => {
                if a[1] != 0 {
                    e.mem.write(a[1], 4, (sz >> 32) as u32)?;
                }
                Ok(sz as u32)
            }
            Err(c) => fail(e, c, INFINITE),
        }),
        "SetEndOfFile" => (1, |e, a| ok_or_err(e, a[0], |v, h| v.set_end_of_file(h))),
        "FlushFileBuffers" => (1, |_, _| Ok(1)),
        "LockFile" | "UnlockFile" => (5, |_, _| Ok(1)),
        "CloseHandle" => (1, |e, a| {
            let h = a[0];
            if (0x10..=0x12).contains(&h) || e.win.vfs.close(h) {
                return Ok(1);
            }
            // Handles we never issued (pseudo handles, emulated objects) close quietly;
            // a closed-twice object handle is an error.
            if (0x1000..0x8000_0000).contains(&h) {
                fail(e, ERROR_INVALID_HANDLE, 0)
            } else {
                Ok(1)
            }
        }),
        "GetFileType" => (1, |e, a| {
            Ok(if (0x10..=0x12).contains(&a[0]) {
                2
            } else if matches!(e.win.vfs.get(a[0]), Some(Obj::File { .. })) {
                1
            } else {
                0
            })
        }),
        "GetFileAttributesA" => (1, |e, a| get_attributes(e, a[0], false)),
        "GetFileAttributesW" => (1, |e, a| get_attributes(e, a[0], true)),
        "SetFileAttributesA" | "SetFileAttributesW" => (2, |_, _| Ok(1)),
        "GetFileTime" => (4, get_file_time),
        "SetFileTime" => (4, |_, _| Ok(1)),
        "GetFileInformationByHandle" => (2, get_file_information),
        "CreateFileMappingA" | "CreateFileMappingW" => (6, create_file_mapping),
        "MapViewOfFile" => (5, map_view_of_file),
        "UnmapViewOfFile" => (1, |_, _| Ok(1)),

        // ---- searches & paths ----
        "FindFirstFileA" => (2, |e, a| find_first(e, a, false)),
        "FindFirstFileW" => (2, |e, a| find_first(e, a, true)),
        "FindNextFileA" => (2, |e, a| find_next(e, a, false)),
        "FindNextFileW" => (2, |e, a| find_next(e, a, true)),
        "FindClose" => (1, |e, a| Ok(e.win.vfs.close(a[0]) as u32)),
        "CreateDirectoryA" => (2, |e, a| path_op(e, a[0], false, |v, p| v.mkdir(p))),
        "CreateDirectoryW" => (2, |e, a| path_op(e, a[0], true, |v, p| v.mkdir(p))),
        "RemoveDirectoryA" => (1, |e, a| path_op(e, a[0], false, |v, p| v.rmdir(p))),
        "RemoveDirectoryW" => (1, |e, a| path_op(e, a[0], true, |v, p| v.rmdir(p))),
        "DeleteFileA" => (1, |e, a| path_op(e, a[0], false, |v, p| v.delete(p))),
        "DeleteFileW" => (1, |e, a| path_op(e, a[0], true, |v, p| v.delete(p))),
        "MoveFileA" => (2, |e, a| move_file(e, a, false)),
        "MoveFileW" => (2, |e, a| move_file(e, a, true)),
        "GetFullPathNameA" => (4, |e, a| full_path_name(e, a, false)),
        "GetFullPathNameW" => (4, |e, a| full_path_name(e, a, true)),
        "SetCurrentDirectoryA" => (1, |e, a| set_current_directory(e, a[0], false)),
        "SetCurrentDirectoryW" => (1, |e, a| set_current_directory(e, a[0], true)),
        "GetCurrentDirectoryA" => (2, |e, a| {
            put_path(e, a[1], a[0], &e.win.vfs.cwd.clone(), false)
        }),
        "GetCurrentDirectoryW" => (2, |e, a| {
            put_path(e, a[1], a[0], &e.win.vfs.cwd.clone(), true)
        }),
        "GetModuleFileNameA" => (3, |e, a| module_file_name(e, a, false)),
        "GetModuleFileNameW" => (3, |e, a| module_file_name(e, a, true)),
        "GetModuleHandleW" => (1, get_module_handle_w),
        "LoadLibraryExA" => (3, |e, a| {
            let name = e.mem.read_cstr(a[0])?;
            e.load_library(&name)
        }),
        "GetDriveTypeA" => (1, |e, a| drive_type(e, a[0], false)),
        "GetDriveTypeW" => (1, |e, a| drive_type(e, a[0], true)),
        "GetLogicalDrives" => (0, |e, _| {
            let cd = e.win.vfs.cd_drive().map_or(0, |d| 1 << (d as u8 - b'A'));
            Ok(1 << 2 | cd) // C:, and the CD-ROM drive
        }),
        "GetLogicalDriveStringsA" => (2, |e, a| {
            let mut drives = b"C:\\\0".to_vec();
            if let Some(d) = e.win.vfs.cd_drive() {
                drives.extend_from_slice(format!("{d}:\\\0").as_bytes());
            }
            drives.push(0);
            if (a[0] as usize) < drives.len() {
                return Ok(drives.len() as u32);
            }
            e.mem.write_bytes(a[1], &drives)?;
            Ok(drives.len() as u32 - 1)
        }),
        "GetComputerNameA" => (2, |e, a| name_out(e, a[0], a[1], "XPMAC")),
        "GlobalHandle" => (1, |_, a| Ok(a[0])),
        "OpenMutexA" => (3, |e, _| fail(e, 2, 0)), // ERROR_FILE_NOT_FOUND: no such mutex yet
        // 16-bit-era file calls (HFILE_ERROR = -1).
        "_lopen" => (2, |e, a| {
            let path = e.mem.read_cstr(a[0])?;
            let access = match a[1] & 3 {
                0 => 0x8000_0000,
                1 => 0x4000_0000,
                _ => 0xC000_0000,
            };
            match e.win.vfs.open(&path, access, 3) {
                Ok(h) => Ok(h),
                Err(c) => fail(e, c, u32::MAX),
            }
        }),
        "_lread" => (3, |e, a| match e.win.vfs.read(a[0], a[2] as usize) {
            Ok(data) => {
                e.mem.write_bytes(a[1], &data)?;
                Ok(data.len() as u32)
            }
            Err(c) => fail(e, c, u32::MAX),
        }),
        "_lclose" => (1, |e, a| {
            Ok(if e.win.vfs.close(a[0]) { 0 } else { u32::MAX })
        }),
        "GetDiskFreeSpaceA" => (5, |e, a| {
            for (slot, v) in a[1..5].iter().zip([8u32, 512, 1_000_000, 2_000_000]) {
                if *slot != 0 {
                    e.mem.write(*slot, 4, v)?;
                }
            }
            Ok(1)
        }),

        // ---- file times ----
        "CompareFileTime" => (2, |e, a| {
            let (x, y) = (read_u64(e, a[0])?, read_u64(e, a[1])?);
            Ok(match x.cmp(&y) {
                std::cmp::Ordering::Less => u32::MAX,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }),
        "FileTimeToLocalFileTime" | "LocalFileTimeToFileTime" => (2, |e, a| {
            let t = read_u64(e, a[0])?;
            write_u64(e, a[1], t)?;
            Ok(1)
        }),
        "FileTimeToSystemTime" => (2, |e, a| {
            let t = read_u64(e, a[0])?;
            write_system_time(e, a[1], t)?;
            Ok(1)
        }),
        "SystemTimeToFileTime" => (2, |e, a| {
            let t = read_system_time(e, a[0])?;
            write_u64(e, a[1], t)?;
            Ok(1)
        }),
        "SetLocalTime" => (1, |_, _| Ok(1)),

        // ---- strings & locale ----
        "CompareStringA" => (6, |e, a| compare_string(e, a, false)),
        "CompareStringW" => (6, |e, a| compare_string(e, a, true)),
        "LCMapStringA" => (6, |e, a| lc_map_string(e, a, false)),
        "LCMapStringW" => (6, |e, a| lc_map_string(e, a, true)),
        "GetStringTypeA" | "GetStringTypeExA" => {
            (5, |e, a| get_string_type(e, a[2], a[3] as i32, a[4], false))
        }
        "GetStringTypeW" => (4, |e, a| get_string_type(e, a[1], a[2] as i32, a[3], true)),
        "GetLocaleInfoA" => (4, |e, a| get_locale_info(e, a, false)),
        "GetLocaleInfoW" => (4, |e, a| get_locale_info(e, a, true)),
        "GetCPInfo" => (2, |e, a| {
            e.mem.write_bytes(a[1], &[0; 20])?;
            e.mem.write(a[1], 4, 1)?; // MaxCharSize
            e.mem.write(a[1] + 4, 1, b'?' as u32)?;
            Ok(1)
        }),
        "GetOEMCP" | "GetConsoleCP" | "GetConsoleOutputCP" => (0, |_, _| Ok(437)),
        "IsValidCodePage" => (1, |_, _| Ok(1)),
        "IsValidLocale" | "EnumSystemLocalesA" => (2, |_, _| Ok(1)),
        "GetUserDefaultLCID" => (0, |_, _| Ok(0x409)),
        "IsDBCSLeadByte" => (1, |_, _| Ok(0)),
        "IsDBCSLeadByteEx" => (2, |_, _| Ok(0)),
        "GetDateFormatA" => (6, |e, a| put_ansi(e, a[4], a[5], "1/1/2005")),
        "GetTimeFormatA" => (6, |e, a| put_ansi(e, a[4], a[5], "12:00:00 PM")),
        "lstrcpyW" => (2, |e, a| {
            let s = e.mem.read_wstr(a[1])?;
            e.mem.write_wstr(a[0], &s)?;
            Ok(a[0])
        }),
        "GetTimeZoneInformation" => (1, |e, a| {
            e.mem.write_bytes(a[0], &[0; 172])?;
            Ok(0) // TIME_ZONE_ID_UNKNOWN, UTC
        }),

        // ---- environment ----
        "GetEnvironmentVariableA" => (3, |e, a| get_env(e, a, false)),
        "GetEnvironmentVariableW" => (3, |e, a| get_env(e, a, true)),
        "SetEnvironmentVariableA" => (2, |e, a| set_env(e, a, false)),
        "SetEnvironmentVariableW" => (2, |e, a| set_env(e, a, true)),
        "GetEnvironmentStrings" => (0, |e, _| env_block(e, false)),
        "GetEnvironmentStringsW" => (0, |e, _| env_block(e, true)),
        "FreeEnvironmentStringsA" | "FreeEnvironmentStringsW" => (1, |_, _| Ok(1)),

        // ---- INI files ----
        "GetPrivateProfileStringA" => (6, |e, a| {
            profile_string(e, a[0], a[1], a[2], a[3], a[4], Some(a[5]))
        }),
        "GetProfileStringA" => (5, |e, a| {
            profile_string(e, a[0], a[1], a[2], a[3], a[4], None)
        }),
        "GetPrivateProfileIntA" => (4, |e, a| {
            let text = read_ini(e, Some(a[3]))?;
            let (app, key) = (e.mem.read_cstr(a[0])?, e.mem.read_cstr(a[1])?);
            Ok(ini_lookup(&text, &app, &key)
                .and_then(|v| v.trim().parse::<i32>().ok())
                .map_or(a[2], |v| v as u32))
        }),
        "WritePrivateProfileStringA" => (4, |_, _| Ok(1)),

        // ---- events, mutexes, waiting, threads ----
        "CreateEventA" | "CreateEventW" => {
            (4, |e, a| Ok(e.win.vfs.create_event(a[1] != 0, a[2] != 0)))
        }
        "SetEvent" => (1, |e, a| set_event(e, a[0], true)),
        "ResetEvent" => (1, |e, a| set_event(e, a[0], false)),
        "CreateMutexA" | "CreateMutexW" => (3, |e, a| {
            Ok(e.win
                .vfs
                .create_mutex(if a[1] != 0 { MAIN_THREAD } else { 0 }))
        }),
        "ReleaseMutex" => (1, |e, a| {
            if let Some(Obj::Mutex { owner, count }) = e.win.vfs.get_mut(a[0]) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    *owner = 0;
                }
            }
            Ok(1)
        }),
        "GetCurrentThread" => (0, |_, _| Ok(0xFFFF_FFFE)),
        "SetThreadPriority" => (2, |_, _| Ok(1)),
        "GetPriorityClass" => (1, |_, _| Ok(0x20)),
        "SetPriorityClass" => (2, |_, _| Ok(1)),
        "OpenProcess" => (3, |_, _| Ok(0x7000)),
        "GetExitCodeProcess" => (2, |e, a| {
            e.mem.write(a[1], 4, 259)?; // STILL_ACTIVE
            Ok(1)
        }),
        "DuplicateHandle" => (7, |e, a| {
            if a[3] != 0 {
                e.mem.write(a[3], 4, a[1])?;
            }
            Ok(1)
        }),
        "CreateProcessA" | "CreateProcessW" => (10, |e, _| fail(e, ERROR_FILE_NOT_FOUND, 0)),
        "CreatePipe" => (4, |e, _| fail(e, ERROR_ACCESS_DENIED, 0)),
        "PeekNamedPipe" => (6, |e, _| fail(e, ERROR_ACCESS_DENIED, 0)),
        "OpenFile" => (3, |e, _| fail(e, ERROR_FILE_NOT_FOUND, INVALID_HANDLE)),
        "SetStdHandle" => (2, |_, _| Ok(1)),
        "SetHandleCount" => (1, |_, a| Ok(a[0])),

        // ---- structured exception handling (see `seh`) ----
        "RaiseException" => (4, crate::win32::seh::raise_exception),
        "RtlUnwind" => (4, crate::win32::seh::rtl_unwind),
        "UnhandledExceptionFilter" => (1, |_, _| Ok(1)),
        "DebugBreak" => (0, |_, _| Ok(0)),
        "OutputDebugStringW" => (1, |_, _| Ok(0)),
        "FatalAppExitA" => (2, |_, _| Err(Stop::Exit(1))),

        // ---- console ----
        "GetConsoleMode" | "GetNumberOfConsoleInputEvents" => {
            (2, |e, _| fail(e, ERROR_INVALID_HANDLE, 0))
        }
        "PeekConsoleInputA" | "ReadConsoleInputA" | "ReadConsoleInputW" => {
            (4, |e, _| fail(e, ERROR_INVALID_HANDLE, 0))
        }
        "ReadConsoleA" | "ReadConsoleW" => (5, |e, _| fail(e, ERROR_INVALID_HANDLE, 0)),
        "SetConsoleMode" | "SetConsoleCtrlHandler" | "Beep" => (2, |_, _| Ok(1)),
        "WriteConsoleA" => (5, |e, a| {
            let data = e.mem.read_bytes(a[1], a[2])?;
            e.win.stdout.extend_from_slice(&data);
            Ok(1)
        }),
        "WriteConsoleW" => (5, |e, a| {
            let mut s = String::new();
            for i in 0..a[2] {
                s.push(char::from_u32(e.mem.read(a[1] + 2 * i, 2)?).unwrap_or('?'));
            }
            e.win.stdout.extend_from_slice(s.as_bytes());
            Ok(1)
        }),

        // ---- heap & memory odds and ends ----
        "HeapCompact" => (2, |_, _| Ok(0)),
        "HeapLock" | "HeapUnlock" => (1, |_, _| Ok(1)),
        "HeapValidate" => (3, |_, _| Ok(1)),
        "HeapWalk" => (2, |e, _| fail(e, 259, 0)), // ERROR_NO_MORE_ITEMS
        "GetProcessHeaps" => (2, |e, a| {
            if a[0] >= 1 {
                e.mem.write(a[1], 4, PROCESS_HEAP)?;
            }
            Ok(1)
        }),
        "GetProcessWorkingSetSize" => (3, |e, a| {
            e.mem.write(a[1], 4, 1 << 20)?;
            e.mem.write(a[2], 4, 64 << 20)?;
            Ok(1)
        }),
        "GlobalMemoryStatus" => (1, |e, a| {
            for (i, v) in [
                32u32,
                10,
                512 << 20,
                256 << 20,
                1 << 30,
                512 << 20,
                2 << 30,
                1536 << 20,
            ]
            .iter()
            .enumerate()
            {
                e.mem.write(a[0] + 4 * i as u32, 4, *v)?;
            }
            Ok(0)
        }),
        "IsBadReadPtr" | "IsBadWritePtr" => (2, |e, a| Ok(!range_mapped(e, a[0], a[1]) as u32)),
        "IsBadCodePtr" => (1, |e, a| Ok(!range_mapped(e, a[0], 1) as u32)),
        "IsBadStringPtrW" => (2, |e, a| Ok(!range_mapped(e, a[0], 2) as u32)),
        "VirtualProtect" => (4, |e, a| {
            if a[3] != 0 {
                e.mem.write(a[3], 4, 4)?; // previous protection: PAGE_READWRITE
            }
            Ok(1)
        }),
        "VirtualQuery" => (3, |e, a| virtual_query(e, a[0], a[1])),
        "VirtualQueryEx" => (4, |e, a| virtual_query(e, a[1], a[2])),
        "FormatMessageA" => (7, format_message),
        _ => return None,
    };
    Some(r)
}

// ---- helpers ------------------------------------------------------------------------------

fn fail(e: &mut Emu, code: u32, ret: u32) -> Res<u32> {
    e.win.last_error = code;
    Ok(ret)
}

fn ok_or_err(e: &mut Emu, h: u32, f: impl FnOnce(&mut Vfs, u32) -> Result<(), u32>) -> Res<u32> {
    match f(&mut e.win.vfs, h) {
        Ok(()) => Ok(1),
        Err(c) => fail(e, c, 0),
    }
}

/// Copy a name into a buffer whose size (in characters) is at `*size`, as GetComputerNameA and
/// GetUserNameA do; `*size` becomes the length written.
pub(crate) fn name_out(e: &mut Emu, buf: u32, size: u32, name: &str) -> Res<u32> {
    let room = e.mem.read(size, 4)?;
    if room as usize <= name.len() {
        e.mem.write(size, 4, name.len() as u32 + 1)?;
        return fail(e, 111, 0); // ERROR_BUFFER_OVERFLOW
    }
    e.mem.write_bytes(buf, name.as_bytes())?;
    e.mem.write(buf + name.len() as u32, 1, 0)?;
    e.mem.write(size, 4, name.len() as u32)?;
    Ok(1)
}

fn path_arg(e: &Emu, p: u32, wide: bool) -> Res<String> {
    Ok(if wide {
        e.mem.read_wstr(p)?
    } else {
        e.mem.read_cstr(p)?
    })
}

fn path_op(
    e: &mut Emu,
    p: u32,
    wide: bool,
    f: impl FnOnce(&Vfs, &str) -> Result<(), u32>,
) -> Res<u32> {
    let path = path_arg(e, p, wide)?;
    match f(&e.win.vfs, &path) {
        Ok(()) => Ok(1),
        Err(c) => fail(e, c, 0),
    }
}

/// Write `s` to a guest buffer like GetCurrentDirectory: returns the length, or the size
/// needed (including NUL) when the buffer is too small.
fn put_path(e: &mut Emu, buf: u32, size: u32, s: &str, wide: bool) -> Res<u32> {
    let len = if wide {
        s.encode_utf16().count()
    } else {
        s.len()
    } as u32;
    if len + 1 > size {
        return Ok(len + 1);
    }
    if wide {
        e.mem.write_wstr(buf, s)?;
    } else {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        e.mem.write_bytes(buf, &b)?;
    }
    Ok(len)
}

fn put_ansi(e: &mut Emu, buf: u32, size: u32, s: &str) -> Res<u32> {
    if size == 0 {
        return Ok(s.len() as u32 + 1);
    }
    put_path(e, buf, size, s, false).map(|n| if n > size { 0 } else { n + 1 })
}

fn read_u64(e: &Emu, p: u32) -> Res<u64> {
    Ok(e.mem.read(p, 4)? as u64 | (e.mem.read(p + 4, 4)? as u64) << 32)
}

fn write_u64(e: &mut Emu, p: u32, v: u64) -> Res<()> {
    e.mem.write(p, 4, v as u32)?;
    e.mem.write(p + 4, 4, (v >> 32) as u32)?;
    Ok(())
}

fn filetime_from_unix(secs: u64) -> u64 {
    (secs + 11_644_473_600) * 10_000_000
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + (m <= 2) as i64, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - (m <= 2) as i64;
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn write_system_time(e: &mut Emu, p: u32, filetime: u64) -> Res<()> {
    let secs = (filetime / 10_000_000) as i64 - 11_644_473_600;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let dow = (days + 4).rem_euclid(7) as u32; // 1970-01-01 was a Thursday
    let ms = ((filetime / 10_000) % 1000) as u32;
    for (i, v) in [
        y as u32,
        m,
        dow,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
        ms,
    ]
    .iter()
    .enumerate()
    {
        e.mem.write(p + 2 * i as u32, 2, *v)?;
    }
    Ok(())
}

fn read_system_time(e: &Emu, p: u32) -> Res<u64> {
    let f = |i: u32| e.mem.read(p + 2 * i, 2);
    let (y, m, d, h, mi, s, ms) = (f(0)?, f(1)?, f(3)?, f(4)?, f(5)?, f(6)?, f(7)?);
    let secs =
        days_from_civil(y as i64, m.max(1), d.max(1)) * 86_400 + (h * 3600 + mi * 60 + s) as i64;
    Ok((secs + 11_644_473_600) as u64 * 10_000_000 + ms as u64 * 10_000)
}

// ---- file calls ---------------------------------------------------------------------------

fn create_file(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let path = path_arg(e, a[0], wide)?;
    match e.win.vfs.open(&path, a[1], a[4]) {
        Ok(h) => Ok(h),
        Err(c) => fail(e, c, INVALID_HANDLE),
    }
}

fn read_file(e: &mut Emu, a: &Args) -> Res<u32> {
    let (h, buf, n, got) = (a[0], a[1], a[2], a[3]);
    if h == 0x10 {
        // stdin: always at end of file
        if got != 0 {
            e.mem.write(got, 4, 0)?;
        }
        return Ok(1);
    }
    match e.win.vfs.read(h, n as usize) {
        Ok(data) => {
            e.mem.write_bytes(buf, &data)?;
            if got != 0 {
                e.mem.write(got, 4, data.len() as u32)?;
            }
            Ok(1)
        }
        Err(c) => fail(e, c, 0),
    }
}

fn set_file_pointer(e: &mut Emu, a: &Args) -> Res<u32> {
    let high = if a[2] != 0 {
        e.mem.read(a[2], 4)? as u64
    } else {
        0
    };
    let offset = if a[2] != 0 {
        ((high << 32) | a[1] as u64) as i64
    } else {
        a[1] as i32 as i64
    };
    match e.win.vfs.seek(a[0], offset, a[3]) {
        Ok(pos) => {
            if a[2] != 0 {
                e.mem.write(a[2], 4, (pos >> 32) as u32)?;
            }
            Ok(pos as u32)
        }
        Err(c) => fail(e, c, INFINITE),
    }
}

fn get_attributes(e: &mut Emu, p: u32, wide: bool) -> Res<u32> {
    let path = path_arg(e, p, wide)?;
    match e.win.vfs.attributes(&path) {
        Some(a) => Ok(a),
        None => {
            let code = if e.win.vfs.resolve(&path).is_ok() {
                ERROR_FILE_NOT_FOUND
            } else {
                ERROR_PATH_NOT_FOUND
            };
            fail(e, code, INFINITE)
        }
    }
}

fn get_file_time(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(path) = e.win.vfs.file_path(a[0]).map(|p| p.to_path_buf()) else {
        return fail(e, ERROR_INVALID_HANDLE, 0);
    };
    let secs = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    for slot in &a[1..4] {
        if *slot != 0 {
            write_u64(e, *slot, filetime_from_unix(secs))?;
        }
    }
    Ok(1)
}

fn get_file_information(e: &mut Emu, a: &Args) -> Res<u32> {
    let size = match e.win.vfs.file_size(a[0]) {
        Ok(s) => s,
        Err(c) => return fail(e, c, 0),
    };
    e.mem.write_bytes(a[1], &[0; 52])?;
    e.mem.write(a[1], 4, FILE_ATTRIBUTE_NORMAL)?;
    e.mem.write(a[1] + 32, 4, (size >> 32) as u32)?;
    e.mem.write(a[1] + 36, 4, size as u32)?;
    e.mem.write(a[1] + 40, 4, 1)?;
    Ok(1)
}

fn create_file_mapping(e: &mut Emu, a: &Args) -> Res<u32> {
    let size = ((a[3] as u64) << 32) | a[4] as u64;
    if a[0] == INVALID_HANDLE {
        return Ok(e.win.vfs.create_anonymous_mapping(size));
    }
    match e.win.vfs.create_mapping(a[0], size) {
        Ok(h) => Ok(h),
        Err(c) => fail(e, c, 0),
    }
}

/// MapViewOfFile: the file's bytes are copied into fresh guest memory (read-only semantics).
fn map_view_of_file(e: &mut Emu, a: &Args) -> Res<u32> {
    let offset = ((a[2] as u64) << 32) | a[3] as u64;
    let Some((path, total)) = (match e.win.vfs.get(a[0]) {
        Some(Obj::Mapping { path, size }) => Some((path.clone(), *size)),
        _ => None,
    }) else {
        return fail(e, ERROR_INVALID_HANDLE, 0);
    };
    let len = if a[4] != 0 {
        a[4] as u64
    } else {
        total.saturating_sub(offset)
    };
    let base = e.alloc_virtual(len as u32);
    if !path.as_os_str().is_empty() {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path)
            .map_err(|_| Stop::Unimplemented("MapViewOfFile: cannot reopen file".into()))?;
        f.seek(SeekFrom::Start(offset)).ok();
        let mut buf = vec![0u8; len as usize];
        let mut got = 0;
        while got < buf.len() {
            match f.read(&mut buf[got..]) {
                Ok(0) | Err(_) => break,
                Ok(k) => got += k,
            }
        }
        e.mem.write_bytes(base, &buf[..got])?;
    }
    Ok(base)
}

// ---- searches & paths ---------------------------------------------------------------------

fn write_find_data(e: &mut Emu, out: u32, entry: &FindEntry, wide: bool) -> Res<()> {
    let size = if wide { 592 } else { 318 };
    e.mem.write_bytes(out, &vec![0; size])?;
    let attrs = if entry.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    e.mem.write(out, 4, attrs)?;
    let ft = filetime_from_unix(entry.mtime);
    for off in [4, 12, 20] {
        write_u64(e, out + off, ft)?;
    }
    e.mem.write(out + 28, 4, (entry.size >> 32) as u32)?;
    e.mem.write(out + 32, 4, entry.size as u32)?;
    if wide {
        e.mem.write_wstr(out + 44, &entry.name)?;
        e.mem.write_wstr(out + 564, &entry.short_name)?;
    } else {
        let mut b = entry.name.as_bytes().to_vec();
        b.truncate(259);
        b.push(0);
        e.mem.write_bytes(out + 44, &b)?;
        let mut short = entry.short_name.as_bytes().to_vec();
        short.push(0);
        e.mem.write_bytes(out + 304, &short)?;
    }
    Ok(())
}

/// GetDriveType: `DRIVE_CDROM` for the CD-ROM drive's paths, otherwise `DRIVE_FIXED`. No path
/// means the current directory's drive.
fn drive_type(e: &mut Emu, path: u32, wide: bool) -> Res<u32> {
    let path = if path == 0 {
        e.win.vfs.cwd.clone()
    } else {
        path_arg(e, path, wide)?
    };
    let drive = path.chars().next().map(|c| c.to_ascii_uppercase());
    Ok(if drive.is_some() && drive == e.win.vfs.cd_drive() {
        5
    } else {
        3
    })
}

fn find_first(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let pattern = path_arg(e, a[0], wide)?;
    match e.win.vfs.find_first(&pattern) {
        Ok((h, first)) => {
            write_find_data(e, a[1], &first, wide)?;
            Ok(h)
        }
        Err(c) => fail(e, c, INVALID_HANDLE),
    }
}

fn find_next(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    match e.win.vfs.find_next(a[0]) {
        Some(entry) => {
            write_find_data(e, a[1], &entry, wide)?;
            Ok(1)
        }
        None => fail(e, ERROR_NO_MORE_FILES, 0),
    }
}

fn move_file(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let (from, to) = (path_arg(e, a[0], wide)?, path_arg(e, a[1], wide)?);
    match e.win.vfs.rename(&from, &to) {
        Ok(()) => Ok(1),
        Err(c) => fail(e, c, 0),
    }
}

fn full_path_name(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let path = path_arg(e, a[0], wide)?;
    let full = e.win.vfs.normalize(&path);
    let n = put_path(e, a[2], a[1], &full, wide)?;
    if a[3] != 0 && n < a[1] {
        let unit = if wide { 2 } else { 1 };
        let tail = full.rsplit_once('\\').map_or(0, |(_, t)| t.len()) as u32;
        e.mem.write(a[3], 4, a[2] + (n - tail) * unit)?;
    }
    Ok(n)
}

fn set_current_directory(e: &mut Emu, p: u32, wide: bool) -> Res<u32> {
    let path = path_arg(e, p, wide)?;
    if e.win.vfs.set_cwd(&path) {
        Ok(1)
    } else {
        fail(e, ERROR_PATH_NOT_FOUND, 0)
    }
}

fn module_file_name(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let path = match e.win.module_by_handle(a[0]) {
        Some(m) if a[0] != e.win.image_base && !m.path.is_empty() => m.path.clone(),
        Some(m) if a[0] != e.win.image_base => {
            let dir = e.win.exe_path.rsplit_once('\\').map_or(r"C:\", |(d, _)| d);
            format!("{dir}\\{}.dll", m.name)
        }
        _ => e.win.exe_path.clone(),
    };
    put_path(e, a[1], a[2], &path, wide)
}

fn get_module_handle_w(e: &mut Emu, a: &Args) -> Res<u32> {
    if a[0] == 0 {
        return Ok(e.win.image_base);
    }
    let name = e.mem.read_wstr(a[0])?;
    if let Some(m) = e.win.find_module(&name) {
        return Ok(m.handle);
    }
    Ok(if crate::win32::Win32::is_emulated_dll(&name) {
        e.win.emulated_handle(&name)
    } else {
        0
    })
}

// ---- strings & locale ---------------------------------------------------------------------

fn read_counted(e: &Emu, p: u32, n: i32, wide: bool) -> Res<Vec<u32>> {
    let mut out = Vec::new();
    let mut i = 0u32;
    loop {
        if n >= 0 && i >= n as u32 {
            break;
        }
        let c = if wide {
            e.mem.read(p + 2 * i, 2)?
        } else {
            e.mem.read(p + i, 1)?
        };
        if n < 0 && c == 0 {
            break;
        }
        out.push(c);
        i += 1;
    }
    Ok(out)
}

fn compare_string(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let (x, y) = (
        read_counted(e, a[2], a[3] as i32, wide)?,
        read_counted(e, a[4], a[5] as i32, wide)?,
    );
    let fold = |v: Vec<u32>| -> Vec<u32> {
        if a[1] & 1 != 0 {
            v.into_iter()
                .map(|c| {
                    char::from_u32(c).map_or(c, |ch| ch.to_lowercase().next().unwrap_or(ch) as u32)
                })
                .collect()
        } else {
            v
        }
    };
    Ok(match fold(x).cmp(&fold(y)) {
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Equal => 2,
        std::cmp::Ordering::Greater => 3,
    })
}

fn lc_map_string(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    const LOWER: u32 = 0x100;
    const UPPER: u32 = 0x200;
    let src_nul_counted = a[3] as i32 == -1;
    let mut chars = read_counted(e, a[2], a[3] as i32, wide)?;
    if src_nul_counted {
        chars.push(0);
    }
    for c in chars.iter_mut() {
        if let Some(ch) = char::from_u32(*c).filter(|_| *c < 0x10000) {
            let mapped = if a[1] & LOWER != 0 {
                ch.to_lowercase().next()
            } else if a[1] & UPPER != 0 {
                ch.to_uppercase().next()
            } else {
                None
            };
            if let Some(m) = mapped.filter(|m| (*m as u32) < if wide { 0x10000 } else { 0x100 }) {
                *c = m as u32;
            }
        }
    }
    if a[5] == 0 {
        return Ok(chars.len() as u32);
    }
    if (a[5] as usize) < chars.len() {
        return fail(e, 122, 0);
    }
    for (i, c) in chars.iter().enumerate() {
        if wide {
            e.mem.write(a[4] + 2 * i as u32, 2, *c)?;
        } else {
            e.mem.write(a[4] + i as u32, 1, *c)?;
        }
    }
    Ok(chars.len() as u32)
}

/// CT_CTYPE1 classification for each character (ASCII precise, others by Unicode class).
fn get_string_type(e: &mut Emu, src: u32, n: i32, out: u32, wide: bool) -> Res<u32> {
    let chars = read_counted(e, src, n, wide)?;
    for (i, c) in chars.iter().enumerate() {
        let ch = char::from_u32(*c).unwrap_or('\0');
        let mut t = 0u32;
        if ch.is_uppercase() {
            t |= 0x1;
        }
        if ch.is_lowercase() {
            t |= 0x2;
        }
        if ch.is_ascii_digit() {
            t |= 0x4;
        }
        if ch.is_whitespace() {
            t |= 0x8;
        }
        if ch.is_ascii_punctuation() {
            t |= 0x10;
        }
        if ch.is_control() {
            t |= 0x20;
        }
        if ch == ' ' || ch == '\t' {
            t |= 0x40;
        }
        if ch.is_ascii_hexdigit() {
            t |= 0x80;
        }
        if ch.is_alphabetic() {
            t |= 0x100;
        }
        e.mem.write(out + 2 * i as u32, 2, t)?;
    }
    Ok(1)
}

fn get_locale_info(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let text = match a[1] & 0xFFFF {
        0x0001 => "0409",
        0x0002 | 0x1002 => "United States",
        0x0003 | 0x1001 => "English",
        0x000B => "437",
        0x000E => ".",
        0x000F => ",",
        0x0010 => "",
        0x0014 | 0x1004 => "1252",
        0x001D => "/",
        0x001E => ":",
        0x0021 | 0x0022 => "0",
        _ => "",
    };
    if a[3] == 0 {
        return Ok(text.len() as u32 + 1);
    }
    put_path(e, a[2], a[3], text, wide).map(|n| if n > a[3] { 0 } else { n + 1 })
}

// ---- environment & INI --------------------------------------------------------------------

fn get_env(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let name = path_arg(e, a[0], wide)?.to_ascii_uppercase();
    match e.win.env.get(&name).cloned() {
        Some(v) => put_path(e, a[1], a[2], &v, wide),
        None => fail(e, 203, 0),
    }
}

fn set_env(e: &mut Emu, a: &Args, wide: bool) -> Res<u32> {
    let name = path_arg(e, a[0], wide)?.to_ascii_uppercase();
    if a[1] == 0 {
        e.win.env.remove(&name);
    } else {
        let v = path_arg(e, a[1], wide)?;
        e.win.env.insert(name, v);
    }
    Ok(1)
}

fn env_block(e: &mut Emu, wide: bool) -> Res<u32> {
    let entries: Vec<String> = e.win.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut units: Vec<u16> = Vec::new();
    for s in &entries {
        units.extend(s.encode_utf16());
        units.push(0);
    }
    units.push(0);
    let bytes = if wide { units.len() * 2 } else { units.len() };
    let p = e.heap_alloc(bytes as u32 + 4)?;
    for (i, u) in units.iter().enumerate() {
        if wide {
            e.mem.write(p + 2 * i as u32, 2, *u as u32)?;
        } else {
            e.mem.write(p + i as u32, 1, *u as u32 & 0xff)?;
        }
    }
    Ok(p)
}

fn read_ini(e: &mut Emu, file_ptr: Option<u32>) -> Res<String> {
    let Some(p) = file_ptr else {
        return Ok(String::new());
    };
    let name = e.mem.read_cstr(p)?;
    // A bare name lives in the Windows directory; ours is the current directory's mount.
    let host = e.win.vfs.resolve(&name).ok();
    Ok(host
        .and_then(|h| std::fs::read(h).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default())
}

/// Look up `key` in section `app` of INI text (case-insensitive; `;` comments).
pub fn ini_lookup(text: &str, app: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[').and_then(|l| l.split_once(']')) {
            in_section = sec.0.trim().eq_ignore_ascii_case(app);
        } else if in_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn profile_string(
    e: &mut Emu,
    app: u32,
    key: u32,
    def: u32,
    buf: u32,
    size: u32,
    file: Option<u32>,
) -> Res<u32> {
    let text = read_ini(e, file)?;
    let default = if def != 0 {
        e.mem.read_cstr(def)?
    } else {
        String::new()
    };
    let value = if app != 0 && key != 0 {
        ini_lookup(&text, &e.mem.read_cstr(app)?, &e.mem.read_cstr(key)?).unwrap_or(default)
    } else {
        default
    };
    if size == 0 {
        return Ok(0);
    }
    let mut v = value.into_bytes();
    v.truncate(size as usize - 1);
    let n = v.len() as u32;
    v.push(0);
    e.mem.write_bytes(buf, &v)?;
    Ok(n)
}

// ---- sync ---------------------------------------------------------------------------------

fn set_event(e: &mut Emu, h: u32, on: bool) -> Res<u32> {
    match e.win.vfs.get_mut(h) {
        Some(Obj::Event { signaled, .. }) => {
            *signaled = on;
            Ok(1)
        }
        _ => fail(e, ERROR_INVALID_HANDLE, 0),
    }
}

// ---- memory queries -----------------------------------------------------------------------

fn range_mapped(e: &Emu, addr: u32, len: u32) -> bool {
    len == 0 || (e.mem.is_mapped(addr) && e.mem.is_mapped(addr.wrapping_add(len - 1)))
}

fn virtual_query(e: &mut Emu, addr: u32, out: u32) -> Res<u32> {
    let base = addr & !0xFFF;
    let committed = e.mem.is_mapped(addr);
    let mut size = 0x1000u32;
    // Extend over following pages in the same state (bounded to keep the scan cheap).
    while size < 0x4000_0000
        && base
            .checked_add(size)
            .is_some_and(|next| e.mem.is_mapped(next) == committed)
    {
        size += 0x1000;
    }
    let (state, protect) = if committed {
        (0x1000u32, 4u32)
    } else {
        (0x10000, 1)
    };
    for (i, v) in [base, base, protect, size, state, protect, 0x20000]
        .iter()
        .enumerate()
    {
        e.mem.write(out + 4 * i as u32, 4, *v)?;
    }
    Ok(28)
}

fn format_message(e: &mut Emu, a: &Args) -> Res<u32> {
    const ALLOCATE_BUFFER: u32 = 0x100;
    let text = format!("Error {}.\r\n", a[2]);
    if a[0] & ALLOCATE_BUFFER != 0 {
        let p = e.heap_alloc(text.len() as u32 + 1)?;
        let mut b = text.clone().into_bytes();
        b.push(0);
        e.mem.write_bytes(p, &b)?;
        e.mem.write(a[4], 4, p)?;
        return Ok(text.len() as u32);
    }
    put_path(e, a[4], a[5], &text, false)
}

#[cfg(test)]
mod tests;
