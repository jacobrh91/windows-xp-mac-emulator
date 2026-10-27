//! The long tail of non-kernel32 Windows APIs a game engine touches at start-up: user32
//! odds and ends, wsprintf, winsock (reporting "no network"), winmm timers (and "no audio
//! device"), version info, COM, UUIDs. Each does the honest minimum: succeed where success
//! is harmless, and fail with the error Windows would give where a device is missing.

use crate::cpu::{Emu, Res};
use crate::win32::{Args, CDECL, Handler};

const MMSYSERR_NODRIVER: u32 = 6;
const MMSYSERR_BADDEVICEID: u32 = 2;
const MCIERR_DEVICE_NOT_INSTALLED: u32 = 0x115;
const DSERR_NODRIVER: u32 = 0x8878_0078;
const E_FAIL: u32 = 0x8000_4005;
/// Stand-ins for GDI palettes and regions, which nothing draws with.
const PALETTE_HANDLE: u32 = 0x8800_0001;
const REGION_HANDLE: u32 = 0x8800_0002;
const WSAEAFNOSUPPORT: u32 = 10047;
const INVALID_SOCKET: u32 = u32::MAX;

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    let r: (usize, Handler) = match (dll, name) {
        // ---- user32 ----
        ("user32", "BringWindowToTop") | ("user32", "ClipCursor") | ("user32", "DestroyCursor") => {
            (1, |_, _| Ok(1))
        }
        ("user32", "CallNextHookEx") => (4, |_, _| Ok(0)),
        ("user32", "CallWindowProcA") => {
            (5, |e, a| e.call_function(a[0], &[a[1], a[2], a[3], a[4]]))
        }
        ("user32", "ChangeDisplaySettingsA") => (2, |e, a| {
            // A new size (DM_PELSWIDTH | DM_PELSHEIGHT) changes the screen; null restores it.
            if a[0] == 0 {
                e.wm.set_screen(crate::gui::wm::DESKTOP_W, crate::gui::wm::DESKTOP_H);
            } else if e.mem.read(a[0] + 40, 4)? & 0x0018_0000 == 0x0018_0000 {
                let (w, h) = (e.mem.read(a[0] + 108, 4)?, e.mem.read(a[0] + 112, 4)?);
                e.wm.set_screen(w, h);
            }
            Ok(0) // DISP_CHANGE_SUCCESSFUL
        }),
        ("user32", "CharNextA") => (1, |e, a| Ok(a[0] + (e.mem.read_u8(a[0])? != 0) as u32)),
        ("user32", "CharPrevA") => (2, |_, a| Ok(if a[1] > a[0] { a[1] - 1 } else { a[0] })),
        ("user32", "CharUpperA") => (1, char_upper),
        ("user32", "CreateCursor") => (7, |_, _| Ok(0x5100)),
        ("user32", "EnableScrollBar") => (3, |_, _| Ok(1)),
        ("user32", "EnumDisplaySettingsA") => (3, enum_display_settings),
        ("user32", "GetCaretBlinkTime") => (0, |_, _| Ok(530)),
        ("user32", "GetDoubleClickTime") => (0, |_, _| Ok(500)),
        ("user32", "GetClassLongA") => (2, |_, _| Ok(0)),
        ("user32", "MapVirtualKeyA") => (2, |_, a| {
            use crate::gui::keyboard::*;
            Ok(match a[1] {
                0 => scan_code(a[0]).map_or(0, |s| s.0),
                1 | 3 => vk_of_scan(a[0] & 0xFF, a[0] & 0xE000 != 0, a[1] == 3),
                2 => char_of(a[0]),
                _ => 0,
            })
        }),
        ("user32", "GetKeyNameTextA") => (3, |e, a| {
            let scan = a[0] >> 16 & 0xFF;
            let Some(name) = crate::gui::keyboard::key_name(scan, a[0] >> 24 & 1 != 0) else {
                return Ok(0);
            };
            if a[2] == 0 {
                return Ok(0);
            }
            let n = name.len().min(a[2] as usize - 1);
            e.mem.write_bytes(a[1], &name.as_bytes()[..n])?;
            e.mem.write(a[1] + n as u32, 1, 0)?;
            Ok(n as u32)
        }),
        ("user32", "GetClipCursor") => (1, |e, a| {
            let (w, h) = e.wm.screen;
            for (i, v) in [0u32, 0, w, h].iter().enumerate() {
                e.mem.write(a[0] + 4 * i as u32, 4, *v)?;
            }
            Ok(1)
        }),
        ("user32", "GetKeyboardLayout") => (1, |_, _| Ok(0x0409_0409)),
        ("user32", "GetScrollInfo")
        | ("user32", "GetScrollRange")
        | ("user32", "SetScrollRange") => (5, |_, _| Ok(0)),
        ("user32", "GetScrollPos") | ("user32", "SetScrollPos") => (3, |_, _| Ok(0)),
        ("user32", "GetTopWindow") => (1, |e, a| {
            Ok(e.wm
                .windows
                .iter()
                .rev()
                .find(|w| w.parent == a[0])
                .map_or(0, |w| w.hwnd))
        }),
        ("user32", "GetWindow") => (2, get_window),
        ("user32", "GetWindowTextW") => (3, |e, a| {
            let Some(t) = e.wm.window(a[0]).map(|w| w.title.clone()) else {
                return Ok(0);
            };
            let units: Vec<u16> = t
                .encode_utf16()
                .take((a[2] as usize).saturating_sub(1))
                .collect();
            for (i, u) in units.iter().enumerate() {
                e.mem.write(a[1] + 2 * i as u32, 2, *u as u32)?;
            }
            if a[2] > 0 {
                e.mem.write(a[1] + 2 * units.len() as u32, 2, 0)?;
            }
            Ok(units.len() as u32)
        }),
        ("user32", "GetWindowThreadProcessId") => (2, |e, a| {
            if a[1] != 0 {
                e.mem.write(a[1], 4, 0x400)?;
            }
            Ok(0x500)
        }),
        ("user32", "SetRect") => (5, |e, a| {
            for (i, v) in a[1..5].iter().enumerate() {
                e.mem.write(a[0] + 4 * i as u32, 4, *v)?;
            }
            Ok(1)
        }),
        ("user32", "SetWindowsHookExA") => (4, |_, _| Ok(0x9000)),
        ("user32", "UnhookWindowsHookEx") => (1, |_, _| Ok(1)),
        ("user32", "SystemParametersInfoA") => (4, |e, a| {
            if a[0] == 0x30 {
                // SPI_GETWORKAREA
                let (w, h) = e.wm.screen;
                for (i, v) in [0u32, 0, w, h].iter().enumerate() {
                    e.mem.write(a[2] + 4 * i as u32, 4, *v)?;
                }
            }
            Ok(1)
        }),
        ("user32", "CreateDialogIndirectParamA") => (5, |_, _| Ok(0)),
        ("user32", "wsprintfA") => (CDECL | 10, |e, a| {
            let mut next = 2;
            let args = *a;
            let text = c_format(e, a[1], &mut || {
                let v = args.get(next).copied().unwrap_or(0);
                next += 1;
                v
            })?;
            write_limited(e, a[0], &text)
        }),
        ("user32", "wvsprintfA") => (3, |e, a| {
            // The va_list is just consecutive argument words in guest memory.
            let words: Vec<u32> = (0..16)
                .map(|i| e.mem.read(a[2].wrapping_add(4 * i), 4).unwrap_or(0))
                .collect();
            let mut it = words.into_iter();
            let text = c_format(e, a[1], &mut || it.next().unwrap_or(0))?;
            write_limited(e, a[0], &text)
        }),

        ("user32", "DestroyIcon") => (1, |_, _| Ok(1)),
        ("user32", "LoadCursorFromFileA") => (1, |_, _| Ok(0xcf00)),
        ("user32", "GetKeyboardType") => (1, |_, a| {
            Ok(match a[0] {
                0 => 4,  // enhanced 101/102-key
                2 => 12, // function keys
                _ => 0,
            })
        }),
        ("user32", "GetUpdateRect") => (3, |e, a| {
            if a[1] != 0 {
                e.mem.write_bytes(a[1], &[0; 16])?;
            }
            Ok(0)
        }),
        ("user32", "GetUpdateRgn") => (3, |_, _| Ok(1)), // NULLREGION
        ("user32", "GetDlgItemTextA") => (4, |e, a| {
            if a[3] > 0 {
                e.mem.write(a[2], 1, 0)?;
            }
            Ok(0)
        }),
        ("user32", "FlashWindow") => (2, |_, _| Ok(0)),

        // ---- gdi32 ----
        ("gdi32", "CreateFontIndirectA") => (1, |e, a| {
            let h = e.mem.read(a[0], 4)? as i32;
            Ok(e.wm.add_gdi(crate::gui::wm::Gdi::Font {
                px: if h == 0 { 13 } else { h.unsigned_abs() },
            }))
        }),
        ("gdi32", "SetViewportOrgEx") => (4, |e, a| {
            if a[3] != 0 {
                e.mem.write_bytes(a[3], &[0; 8])?;
            }
            Ok(1)
        }),
        // Palettes: the display is true colour, so these only need to be accepted.
        ("gdi32", "CreatePalette") => (1, |_, _| Ok(PALETTE_HANDLE)),
        ("gdi32", "SelectPalette") => (3, |_, _| Ok(PALETTE_HANDLE)),
        ("gdi32", "RealizePalette") => (1, |_, _| Ok(0)),
        ("gdi32", "AnimatePalette") => (4, |_, _| Ok(1)),
        ("gdi32", "SetSystemPaletteUse") | ("gdi32", "GetSystemPaletteUse") => (2, |_, _| Ok(1)), // SYSPAL_STATIC
        ("gdi32", "GetSystemPaletteEntries") => (4, |e, a| {
            for i in 0..a[2] {
                let v = (a[1] + i) & 0xFF;
                e.mem.write(a[3] + 4 * i, 4, v * 0x010101)?;
            }
            Ok(a[2])
        }),
        ("gdi32", "SetDIBColorTable") => (4, |_, a| Ok(a[2])),
        ("gdi32", "GetBkMode") => (1, |_, _| Ok(2)), // OPAQUE
        ("gdi32", "GetBkColor") => (1, |_, _| Ok(0x00FF_FFFF)),
        ("gdi32", "GetTextColor") => (1, |_, _| Ok(0)),
        ("gdi32", "GdiFlush") => (0, |_, _| Ok(1)),
        ("gdi32", "CreateRectRgn") => (4, |_, _| Ok(REGION_HANDLE)),
        ("gdi32", "GetRegionData") => (3, |_, _| Ok(0)),
        ("gdi32", "SetStretchBltMode") => (2, |_, _| Ok(1)),
        ("gdi32", "SetTextAlign") => (2, |_, _| Ok(0)),
        ("gdi32", "GetDIBits") => (7, |_, _| Ok(0)),
        ("gdi32", "CreateDIBSection") => (6, |e, _| {
            e.win.last_error = 87; // ERROR_INVALID_PARAMETER: DIB sections are not supported yet
            Ok(0)
        }),
        ("gdi32", "GetCharWidthA") | ("gdi32", "GetCharWidthW") => (4, |e, a| {
            for (i, c) in (a[1]..=a[2]).enumerate() {
                let ch = char::from_u32(c).unwrap_or('?').to_string();
                let (w, _) = e.wm.measure_text(a[0], &ch);
                e.mem.write(a[3] + 4 * i as u32, 4, w)?;
            }
            Ok(1)
        }),

        // ---- ole32 / rpcrt4 / version / wininet / shlwapi / iphlpapi ----
        ("ole32", "CoInitialize") => (1, |_, _| Ok(0)),
        ("ole32", "CoUninitialize") => (0, |_, _| Ok(0)),
        ("ole32", "CoCreateInstance") => (5, |_, _| Ok(0x8004_0154)), // REGDB_E_CLASSNOTREG
        ("rpcrt4", "UuidCreate") => (1, |e, a| {
            let seed = e
                .win
                .rand_seed
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            e.win.rand_seed = seed;
            for i in 0..4u32 {
                e.mem.write(
                    a[0] + 4 * i,
                    4,
                    seed.rotate_left(i * 8) ^ (0x9E37_79B9u32.wrapping_mul(i + 1)),
                )?;
            }
            Ok(0)
        }),
        ("rpcrt4", "UuidEqual") => (3, |e, a| {
            let same = e.mem.read_bytes(a[0], 16)? == e.mem.read_bytes(a[1], 16)?;
            if a[2] != 0 {
                e.mem.write(a[2], 4, 0)?;
            }
            Ok(same as u32)
        }),
        ("rpcrt4", "UuidToStringA") => (2, |e, a| {
            let b = e.mem.read_bytes(a[0], 16)?;
            let s = format!(
                "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                u16::from_le_bytes([b[4], b[5]]),
                u16::from_le_bytes([b[6], b[7]]),
                b[8],
                b[9],
                b[10],
                b[11],
                b[12],
                b[13],
                b[14],
                b[15]
            );
            let p = e.heap_alloc(s.len() as u32 + 1)?;
            let mut bytes = s.into_bytes();
            bytes.push(0);
            e.mem.write_bytes(p, &bytes)?;
            e.mem.write(a[1], 4, p)?;
            Ok(0)
        }),
        ("rpcrt4", "UuidFromStringA") => (2, |e, a| {
            let s = e.mem.read_cstr(a[0])?.replace('-', "");
            if s.len() != 32 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(1705); // RPC_S_INVALID_STRING_UUID
            }
            let n = |i: usize, len: usize| u64::from_str_radix(&s[i..i + len], 16).unwrap_or(0);
            e.mem.write(a[1], 4, n(0, 8) as u32)?;
            e.mem.write(a[1] + 4, 2, n(8, 4) as u32)?;
            e.mem.write(a[1] + 6, 2, n(12, 4) as u32)?;
            for i in 0..8 {
                e.mem
                    .write(a[1] + 8 + i as u32, 1, n(16 + 2 * i, 2) as u32)?;
            }
            Ok(0)
        }),
        ("rpcrt4", "RpcStringFreeA") => (1, |_, _| Ok(0)),
        ("version", "GetFileVersionInfoSizeA") => (2, |_, _| Ok(0)),
        ("version", "GetFileVersionInfoA") | ("version", "VerQueryValueA") => (4, |_, _| Ok(0)),
        ("wininet", "InternetCloseHandle") => (1, |_, _| Ok(1)),
        ("wininet", "InternetSetStatusCallback") => (2, |_, _| Ok(0)),
        ("shlwapi", "PathFileExistsW") => (1, |e, a| {
            let p = e.mem.read_wstr(a[0])?;
            Ok(e.win.vfs.attributes(&p).is_some() as u32)
        }),
        ("iphlpapi", "GetBestRoute") | ("iphlpapi", "GetIpAddrTable") => (3, |_, _| Ok(50)), // ERROR_NOT_SUPPORTED

        // ---- winmm: real clock, no audio / MIDI / mixer hardware ----
        ("winmm", "timeGetTime") => (0, |e, _| Ok(e.win.elapsed_ms() as u32)),
        ("winmm", "timeBeginPeriod") | ("winmm", "timeEndPeriod") => (1, |_, _| Ok(0)),
        ("winmm", "timeKillEvent") => (1, |_, _| Ok(0)),
        // Timer callbacks do not fire yet; hand back a plausible id so callers carry on.
        ("winmm", "timeSetEvent") => (5, |_, _| Ok(1)),
        ("winmm", "auxGetNumDevs") | ("winmm", "mixerGetNumDevs") => (0, |_, _| Ok(0)),
        ("winmm", "auxGetDevCapsA") | ("winmm", "auxGetVolume") | ("winmm", "auxSetVolume") => {
            (3, |_, _| Ok(MMSYSERR_NODRIVER))
        }
        ("winmm", "mciSendCommandA") => (4, |_, _| Ok(MMSYSERR_NODRIVER)),
        ("winmm", "mciSendStringA") => (4, |e, a| {
            if a[1] != 0 && a[2] > 0 {
                e.mem.write(a[1], 1, 0)?;
            }
            Ok(MCIERR_DEVICE_NOT_INSTALLED)
        }),
        ("winmm", "midiOutGetNumDevs") => (0, |_, _| Ok(0)),
        ("winmm", "midiOutGetDevCapsA") => (3, |_, _| Ok(MMSYSERR_BADDEVICEID)),
        ("winmm", "midiOutGetVolume") | ("winmm", "midiOutSetVolume") => {
            (2, |_, _| Ok(MMSYSERR_NODRIVER))
        }

        // ---- DirectSound, DirectInput, DirectPlay: no such devices (yet) ----
        ("dsound", "DirectSoundCreate") => (3, |_, _| Ok(DSERR_NODRIVER)),
        ("dsound", "DirectSoundEnumerateA") => (2, |_, _| Ok(0)),
        ("dinput", "DirectInputCreateA") => (4, |_, _| Ok(E_FAIL)),
        ("dplayx", "#1") => (3, |_, _| Ok(E_FAIL)), // DirectPlayCreate
        ("dplayx", "#2") => (2, |_, _| Ok(E_FAIL)), // DirectPlayEnumerateA

        // ---- comdlg32 / shell32 ----
        ("comdlg32", "GetOpenFileNameA") | ("comdlg32", "GetSaveFileNameA") => (1, |_, _| Ok(0)), // cancelled
        ("shell32", "DragAcceptFiles") => (2, |_, _| Ok(0)),
        ("shell32", "DragQueryFileA") => (4, |_, _| Ok(0)),
        ("shell32", "Shell_NotifyIconA") => (2, |_, _| Ok(1)),
        ("winmm", "midiOutOpen")
        | ("winmm", "waveOutOpen")
        | ("winmm", "waveInOpen")
        | ("winmm", "mixerOpen") => (6, |_, _| Ok(MMSYSERR_NODRIVER)),
        ("winmm", "midiOutClose")
        | ("winmm", "midiOutReset")
        | ("winmm", "mixerClose")
        | ("winmm", "waveOutClose")
        | ("winmm", "waveOutPause")
        | ("winmm", "waveOutReset")
        | ("winmm", "waveOutRestart")
        | ("winmm", "waveInClose")
        | ("winmm", "waveInReset")
        | ("winmm", "waveInStart") => (1, |_, _| Ok(MMSYSERR_NODRIVER)),
        ("winmm", "midiOutShortMsg")
        | ("winmm", "waveOutGetID")
        | ("winmm", "waveOutSetVolume") => (2, |_, _| Ok(MMSYSERR_NODRIVER)),
        ("winmm", "midiOutLongMsg")
        | ("winmm", "midiOutPrepareHeader")
        | ("winmm", "midiOutUnprepareHeader")
        | ("winmm", "waveOutPrepareHeader")
        | ("winmm", "waveOutUnprepareHeader")
        | ("winmm", "waveOutWrite")
        | ("winmm", "waveInAddBuffer")
        | ("winmm", "waveInPrepareHeader")
        | ("winmm", "waveInUnprepareHeader")
        | ("winmm", "waveOutGetDevCapsA")
        | ("winmm", "mixerGetLineInfoA")
        | ("winmm", "mixerGetLineControlsA")
        | ("winmm", "mixerGetControlDetailsA")
        | ("winmm", "mixerSetControlDetails") => (3, |_, _| Ok(MMSYSERR_NODRIVER)),

        // ---- winsock (ordinals are how the game imports it): no network yet ----
        ("ws2_32", "#115") | ("ws2_32", "WSAStartup") => (2, |e, a| {
            e.mem.write_bytes(a[1], &[0; 400])?;
            e.mem.write(a[1], 2, 0x0202)?; // wVersion
            e.mem.write(a[1] + 2, 2, 0x0202)?; // wHighVersion
            Ok(0)
        }),
        ("ws2_32", "#116") | ("ws2_32", "WSACleanup") => (0, |_, _| Ok(0)),
        ("ws2_32", "#111") | ("ws2_32", "WSAGetLastError") => (0, |e, _| Ok(e.win.last_error)),
        ("ws2_32", "#8") | ("ws2_32", "htonl") | ("ws2_32", "#14") | ("ws2_32", "ntohl") => {
            (1, |_, a| Ok(a[0].swap_bytes()))
        }
        ("ws2_32", "#9") | ("ws2_32", "htons") | ("ws2_32", "#15") | ("ws2_32", "ntohs") => {
            (1, |_, a| Ok((a[0] as u16).swap_bytes() as u32))
        }
        ("ws2_32", "#11") | ("ws2_32", "inet_addr") => (1, |e, a| {
            let s = e.mem.read_cstr(a[0])?;
            let parts: Vec<u8> = s.split('.').filter_map(|p| p.parse().ok()).collect();
            Ok(if parts.len() == 4 {
                u32::from_le_bytes([parts[0], parts[1], parts[2], parts[3]])
            } else {
                u32::MAX
            })
        }),
        ("ws2_32", "#12") | ("ws2_32", "inet_ntoa") => (1, |e, a| {
            let b = a[0].to_le_bytes();
            let s = format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]);
            let p = e.heap_alloc(20)?;
            let mut bytes = s.into_bytes();
            bytes.push(0);
            e.mem.write_bytes(p, &bytes)?;
            Ok(p)
        }),
        ("ws2_32", "#57") | ("ws2_32", "gethostname") => (2, |e, a| {
            e.mem.write_bytes(a[0], b"localhost\0")?;
            Ok(0)
        }),
        ("ws2_32", "#23") | ("ws2_32", "socket") => (3, |e, _| {
            e.win.last_error = WSAEAFNOSUPPORT;
            Ok(INVALID_SOCKET)
        }),
        ("ws2_32", "WSASocketA") => (6, |e, _| {
            e.win.last_error = WSAEAFNOSUPPORT;
            Ok(INVALID_SOCKET)
        }),
        ("ws2_32", "#3")
        | ("ws2_32", "closesocket")
        | ("ws2_32", "#22")
        | ("ws2_32", "shutdown") => (2, |_, _| Ok(0)),
        // Everything else a socket does fails with SOCKET_ERROR (-1) and "not a socket".
        ("ws2_32", "#52") | ("ws2_32", "gethostbyname") => (1, |_, _| Ok(0)),
        ("ws2_32", "#51") | ("ws2_32", "gethostbyaddr") | ("ws2_32", "#55") | ("ws2_32", "#56") => {
            (3, |_, _| Ok(0))
        }
        ("ws2_32", "#1")
        | ("ws2_32", "#2")
        | ("ws2_32", "#4")
        | ("ws2_32", "#5")
        | ("ws2_32", "#6")
        | ("ws2_32", "#10")
        | ("ws2_32", "#13") => {
            (3, |e, _| {
                e.win.last_error = 10038; // WSAENOTSOCK
                Ok(u32::MAX)
            })
        }
        ("ws2_32", "#7") | ("ws2_32", "#21") => (5, |e, _| {
            e.win.last_error = 10038;
            Ok(u32::MAX)
        }),
        ("ws2_32", "#16") | ("ws2_32", "#19") => (4, |e, _| {
            e.win.last_error = 10038;
            Ok(u32::MAX)
        }),
        ("ws2_32", "#17") | ("ws2_32", "#20") => (6, |e, _| {
            e.win.last_error = 10038;
            Ok(u32::MAX)
        }),
        ("ws2_32", "#18") => (5, |_, _| Ok(0)), // select: nothing ready
        ("ws2_32", "#151") => (2, |_, _| Ok(0)), // __WSAFDIsSet
        ("ws2_32", "WSAIoctl") => (9, |e, _| {
            e.win.last_error = 10038;
            Ok(u32::MAX)
        }),

        // ---- imm32 (input method editors): behave as if no IME is installed ----
        ("imm32", "ImmGetContext")
        | ("imm32", "ImmGetOpenStatus")
        | ("imm32", "ImmGetVirtualKey")
        | ("imm32", "ImmLockIMC")
        | ("imm32", "ImmUnlockIMC")
        | ("imm32", "ImmLockIMCC")
        | ("imm32", "ImmUnlockIMCC")
        | ("imm32", "ImmGetDefaultIMEWnd")
        | ("imm32", "ImmIsIME")
        | ("imm32", "ImmDisableTextFrameService") => (1, |_, _| Ok(0)),
        ("imm32", "ImmReleaseContext") | ("imm32", "ImmSetOpenStatus") => (2, |_, _| Ok(1)),
        ("imm32", "ImmAssociateContext") | ("imm32", "ImmSimulateHotKey") => (2, |_, _| Ok(0)),
        ("imm32", "ImmGetConversionStatus")
        | ("imm32", "ImmSetConversionStatus")
        | ("imm32", "ImmGetIMEFileNameA") => (3, |_, _| Ok(0)),
        ("imm32", "ImmNotifyIME")
        | ("imm32", "ImmGetCompositionStringA")
        | ("imm32", "ImmGetCandidateListA") => (4, |_, _| Ok(0)),

        // ---- kernel32 extras looked up by name at run time ----
        // Report no optional CPU features (MMX/SSE/3DNow): the emulator runs plain x86 + x87.
        ("kernel32", "IsProcessorFeaturePresent") => (1, |_, _| Ok(0)),

        // ---- Direct3D 8: not available yet, so the game sees "no device" ----
        _ => return None,
    };
    Some(r)
}

fn char_upper(e: &mut Emu, a: &Args) -> Res<u32> {
    if a[0] < 0x1_0000 {
        // A single character in the low word, not a pointer.
        let c = (a[0] & 0xff) as u8;
        return Ok(c.to_ascii_uppercase() as u32);
    }
    let mut p = a[0];
    loop {
        let b = e.mem.read_u8(p)?;
        if b == 0 {
            break;
        }
        e.mem.write(p, 1, b.to_ascii_uppercase() as u32)?;
        p += 1;
    }
    Ok(a[0])
}

fn get_window(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(w) = e.wm.window(a[0]) else {
        return Ok(0);
    };
    let parent = w.parent;
    let siblings: Vec<u32> =
        e.wm.windows
            .iter()
            .filter(|x| x.parent == parent)
            .map(|x| x.hwnd)
            .collect();
    let pos = siblings.iter().position(|&h| h == a[0]).unwrap_or(0);
    Ok(match a[1] {
        0 => siblings.first().copied().unwrap_or(0), // GW_HWNDFIRST
        1 => siblings.last().copied().unwrap_or(0),  // GW_HWNDLAST
        2 => siblings.get(pos + 1).copied().unwrap_or(0), // GW_HWNDNEXT
        3 => pos
            .checked_sub(1)
            .and_then(|i| siblings.get(i))
            .copied()
            .unwrap_or(0), // GW_HWNDPREV
        5 => {
            e.wm.windows
                .iter()
                .find(|x| x.parent == a[0])
                .map_or(0, |x| x.hwnd)
        } // GW_CHILD
        _ => 0,
    })
}

/// A few standard display modes, then "no more modes".
fn enum_display_settings(e: &mut Emu, a: &Args) -> Res<u32> {
    const MODES: [(u32, u32); 4] = [(640, 480), (800, 600), (1024, 768), (1280, 1024)];
    let (w, h) = match a[1] {
        0xFFFF_FFFF | 0xFFFF_FFFE => e.wm.screen, // ENUM_CURRENT_SETTINGS / ENUM_REGISTRY_SETTINGS
        n if (n as usize) < MODES.len() => MODES[n as usize],
        _ => return Ok(0),
    };
    let dm = a[2];
    e.mem.write_bytes(dm, &[0; 156])?;
    e.mem.write(dm + 36, 2, 156)?; // dmSize
    e.mem.write(dm + 40, 4, 0x005C_0000)?; // dmFields: bpp, width, height, frequency
    e.mem.write(dm + 104, 4, 32)?;
    e.mem.write(dm + 108, 4, w)?;
    e.mem.write(dm + 112, 4, h)?;
    e.mem.write(dm + 120, 4, 60)?;
    Ok(1)
}

fn write_limited(e: &mut Emu, buf: u32, text: &str) -> Res<u32> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.truncate(1023); // wsprintf's documented limit is 1024 bytes
    let n = bytes.len() as u32;
    bytes.push(0);
    e.mem.write_bytes(buf, &bytes)?;
    Ok(n)
}

/// printf-style formatting for the subset wsprintf supports: flags `-0+ #`, width,
/// precision, `l`/`h`, and `d i u x X c s %`. `next` supplies successive argument words.
pub fn c_format(e: &Emu, fmt_ptr: u32, next: &mut dyn FnMut() -> u32) -> Res<String> {
    let fmt = e.mem.read_cstr(fmt_ptr)?;
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let (mut left, mut zero, mut plus, mut space, mut alt) =
            (false, false, false, false, false);
        while let Some(&f) = chars.peek() {
            match f {
                '-' => left = true,
                '0' => zero = true,
                '+' => plus = true,
                ' ' => space = true,
                '#' => alt = true,
                _ => break,
            }
            chars.next();
        }
        let mut width = 0usize;
        if chars.peek() == Some(&'*') {
            chars.next();
            width = next() as usize;
        }
        while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
            width = width * 10 + d as usize;
            chars.next();
        }
        let mut precision: Option<usize> = None;
        if chars.peek() == Some(&'.') {
            chars.next();
            let mut p = 0;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                p = p * 10 + d as usize;
                chars.next();
            }
            precision = Some(p);
        }
        let mut short = false;
        while let Some(&m) = chars.peek() {
            match m {
                'l' | 'L' | 'I' | '3' | '2' => {}
                'h' => short = true,
                _ => break,
            }
            chars.next();
        }
        let Some(conv) = chars.next() else { break };
        let body = match conv {
            '%' => "%".to_string(),
            'c' => char::from_u32(next() & 0xff).unwrap_or('?').to_string(),
            's' => {
                let p = next();
                let s = if p == 0 {
                    "(null)".to_string()
                } else {
                    e.mem.read_cstr(p)?
                };
                match precision {
                    Some(n) => s.chars().take(n).collect(),
                    None => s,
                }
            }
            'd' | 'i' => {
                let raw = next();
                let v = if short {
                    raw as u16 as i16 as i64
                } else {
                    raw as i32 as i64
                };
                let mut digits = v.unsigned_abs().to_string();
                if let Some(p) = precision {
                    while digits.len() < p {
                        digits.insert(0, '0');
                    }
                }
                let sign = if v < 0 {
                    "-"
                } else if plus {
                    "+"
                } else if space {
                    " "
                } else {
                    ""
                };
                pad_number(sign, &digits, width, left, zero && precision.is_none())
            }
            'u' | 'x' | 'X' => {
                let raw = next();
                let v = if short { raw as u16 as u32 } else { raw };
                let mut digits = match conv {
                    'u' => v.to_string(),
                    'x' => format!("{v:x}"),
                    _ => format!("{v:X}"),
                };
                if let Some(p) = precision {
                    while digits.len() < p {
                        digits.insert(0, '0');
                    }
                }
                let prefix = if alt && v != 0 && conv != 'u' {
                    if conv == 'x' { "0x" } else { "0X" }
                } else {
                    ""
                };
                pad_number(prefix, &digits, width, left, zero && precision.is_none())
            }
            other => format!("%{other}"),
        };
        let pad = width.saturating_sub(body.chars().count());
        if left {
            out.push_str(&body);
            out.push_str(&" ".repeat(pad));
        } else if matches!(conv, 'd' | 'i' | 'u' | 'x' | 'X') {
            out.push_str(&body); // numbers are padded by pad_number
        } else {
            out.push_str(&" ".repeat(pad));
            out.push_str(&body);
        }
    }
    Ok(out)
}

/// Sign/prefix + digits, padded to `width` with zeros between sign and digits or with spaces.
fn pad_number(prefix: &str, digits: &str, width: usize, left: bool, zero: bool) -> String {
    let len = prefix.len() + digits.len();
    if len >= width {
        return format!("{prefix}{digits}");
    }
    let pad = width - len;
    if left {
        format!("{prefix}{digits}{}", " ".repeat(pad))
    } else if zero {
        format!("{prefix}{}{digits}", "0".repeat(pad))
    } else {
        format!("{}{prefix}{digits}", " ".repeat(pad))
    }
}

#[cfg(test)]
mod tests;
