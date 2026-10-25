//! The C runtime's (msvcr70) string and memory functions, patched by export name.
//!
//! Standard functions with no locale dependence, so a native version is exact. (The
//! case-insensitive ones follow the C library's locale and are left alone.) A bad pointer
//! faults in the native version just as it would in the original, as an access violation.

use crate::cpu::{Emu, Res};
use crate::win32::Handler;

pub(super) const CRT_EXPORTS: &[(&str, &str)] = &[
    ("strlen", "crt_strlen"),
    ("strcpy", "crt_strcpy"),
    ("strcat", "crt_strcat"),
    ("strcmp", "crt_strcmp"),
    ("strncmp", "crt_strncmp"),
    ("strncpy", "crt_strncpy"),
    ("strchr", "crt_strchr"),
    ("strrchr", "crt_strrchr"),
    ("strstr", "crt_strstr"),
    ("strcspn", "crt_strcspn"),
    ("wcslen", "crt_wcslen"),
    ("wcscpy", "crt_wcscpy"),
    ("wcscmp", "crt_wcscmp"),
    ("wcspbrk", "crt_wcspbrk"),
    ("memcpy", "crt_memmove"),
    ("memmove", "crt_memmove"),
    ("memset", "crt_memset"),
    ("memcmp", "crt_memcmp"),
];

/// The bytes of a NUL-terminated string (without the NUL).
fn cstr(e: &Emu, p: u32) -> Res<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let b = e.mem.read_u8(p.wrapping_add(out.len() as u32))?;
        if b == 0 {
            return Ok(out);
        }
        out.push(b);
    }
}

/// The units of a NUL-terminated wide string (without the NUL).
fn wstr(e: &Emu, p: u32) -> Res<Vec<u16>> {
    let mut out = Vec::new();
    loop {
        let c = e.mem.read(p.wrapping_add(2 * out.len() as u32), 2)? as u16;
        if c == 0 {
            return Ok(out);
        }
        out.push(c);
    }
}

fn sign(o: std::cmp::Ordering) -> u32 {
    o as i32 as u32
}

pub(super) fn lookup(name: &str) -> Option<(usize, Handler)> {
    use crate::win32::CDECL;
    let h: (usize, Handler) = match name {
        "crt_strlen" => (1, |e, a| Ok(cstr(e, a[0])?.len() as u32)),
        "crt_strcpy" => (2, |e, a| {
            let mut s = cstr(e, a[1])?;
            s.push(0);
            e.mem.write_bytes(a[0], &s)?;
            Ok(a[0])
        }),
        "crt_strcat" => (2, |e, a| {
            let end = a[0] + cstr(e, a[0])?.len() as u32;
            let mut s = cstr(e, a[1])?;
            s.push(0);
            e.mem.write_bytes(end, &s)?;
            Ok(a[0])
        }),
        "crt_strcmp" => (2, |e, a| Ok(sign(cstr(e, a[0])?.cmp(&cstr(e, a[1])?)))),
        "crt_strncmp" => (3, |e, a| {
            for i in 0..a[2] {
                let (x, y) = (
                    e.mem.read_u8(a[0].wrapping_add(i))?,
                    e.mem.read_u8(a[1].wrapping_add(i))?,
                );
                if x != y {
                    return Ok((x as i32 - y as i32) as u32);
                }
                if x == 0 {
                    break;
                }
            }
            Ok(0)
        }),
        "crt_strncpy" => (3, |e, a| {
            // Up to n bytes; zero-padded when the source is shorter.
            let mut s = cstr(e, a[1])?;
            s.resize(a[2] as usize, 0);
            e.mem.write_bytes(a[0], &s)?;
            Ok(a[0])
        }),
        "crt_strchr" => (2, |e, a| {
            let (s, c) = (cstr(e, a[0])?, a[1] as u8);
            Ok(if c == 0 {
                a[0] + s.len() as u32
            } else {
                s.iter()
                    .position(|&b| b == c)
                    .map_or(0, |i| a[0] + i as u32)
            })
        }),
        "crt_strrchr" => (2, |e, a| {
            let (s, c) = (cstr(e, a[0])?, a[1] as u8);
            Ok(if c == 0 {
                a[0] + s.len() as u32
            } else {
                s.iter()
                    .rposition(|&b| b == c)
                    .map_or(0, |i| a[0] + i as u32)
            })
        }),
        "crt_strstr" => (2, |e, a| {
            let (h, n) = (cstr(e, a[0])?, cstr(e, a[1])?);
            if n.is_empty() {
                return Ok(a[0]);
            }
            Ok(h.windows(n.len())
                .position(|w| w == n.as_slice())
                .map_or(0, |i| a[0] + i as u32))
        }),
        "crt_strcspn" => (2, |e, a| {
            let (s, set) = (cstr(e, a[0])?, cstr(e, a[1])?);
            Ok(s.iter().position(|b| set.contains(b)).unwrap_or(s.len()) as u32)
        }),
        "crt_wcslen" => (1, |e, a| Ok(wstr(e, a[0])?.len() as u32)),
        "crt_wcscpy" => (2, |e, a| {
            let s = wstr(e, a[1])?;
            for (i, c) in s.iter().chain(std::iter::once(&0)).enumerate() {
                e.mem.write(a[0] + 2 * i as u32, 2, *c as u32)?;
            }
            Ok(a[0])
        }),
        "crt_wcscmp" => (2, |e, a| Ok(sign(wstr(e, a[0])?.cmp(&wstr(e, a[1])?)))),
        "crt_wcspbrk" => (2, |e, a| {
            let (s, set) = (wstr(e, a[0])?, wstr(e, a[1])?);
            Ok(s.iter()
                .position(|c| set.contains(c))
                .map_or(0, |i| a[0] + 2 * i as u32))
        }),
        "crt_memmove" => (3, |e, a| {
            let data = e.mem.read_bytes(a[1], a[2])?;
            e.mem.write_bytes(a[0], &data)?;
            Ok(a[0])
        }),
        "crt_memset" => (3, |e, a| {
            e.mem.write_bytes(a[0], &vec![a[1] as u8; a[2] as usize])?;
            Ok(a[0])
        }),
        "crt_memcmp" => (3, |e, a| {
            Ok(sign(
                e.mem
                    .read_bytes(a[0], a[2])?
                    .cmp(&e.mem.read_bytes(a[1], a[2])?),
            ))
        }),
        _ => return None,
    };
    Some((h.0 | CDECL, h.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::call;

    #[test]
    fn c_runtime_string_functions() {
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x1000);
        let run = |e: &mut Emu, name: &str, args: &[u32]| -> u32 {
            let (argc, h) = lookup(name).unwrap();
            assert_ne!(argc & crate::win32::CDECL, 0, "{name} is cdecl");
            call(e, h, args)
        };
        let put = |e: &mut Emu, at: u32, s: &[u8]| e.mem.write_bytes(at, s).unwrap();
        put(&mut e, 0x10000, b"hello\0");
        put(&mut e, 0x10100, b", world\0");
        assert_eq!(run(&mut e, "crt_strlen", &[0x10000]), 5);
        assert_eq!(run(&mut e, "crt_strcat", &[0x10000, 0x10100]), 0x10000);
        assert_eq!(cstr(&e, 0x10000).unwrap(), b"hello, world");
        assert_eq!(run(&mut e, "crt_strchr", &[0x10000, b'o' as u32]), 0x10004);
        assert_eq!(run(&mut e, "crt_strrchr", &[0x10000, b'o' as u32]), 0x10008);
        assert_eq!(
            run(&mut e, "crt_strchr", &[0x10000, 0]),
            0x1000C,
            "the terminator can be found"
        );
        assert_eq!(run(&mut e, "crt_strstr", &[0x10000, 0x10102]), 0x10007);
        assert_eq!(
            run(&mut e, "crt_strcspn", &[0x10000, 0x10100]),
            2,
            "the set \", world\" includes l"
        );
        put(&mut e, 0x10200, b"abc\0abd\0");
        assert_eq!(run(&mut e, "crt_strcmp", &[0x10200, 0x10204]) as i32, -1);
        assert_eq!(run(&mut e, "crt_strncmp", &[0x10200, 0x10204, 2]), 0);
        run(&mut e, "crt_strncpy", &[0x10300, 0x10200, 6]);
        assert_eq!(
            e.mem.read_bytes(0x10300, 6).unwrap(),
            b"abc\0\0\0",
            "zero-padded"
        );
        run(&mut e, "crt_memset", &[0x10400, 0x7F, 3]);
        run(&mut e, "crt_memmove", &[0x10401, 0x10400, 3]);
        assert_eq!(
            e.mem.read_bytes(0x10400, 4).unwrap(),
            [0x7F; 4],
            "overlapping moves are safe"
        );
        for (i, c) in "wide\0".encode_utf16().enumerate() {
            e.mem.write(0x10500 + 2 * i as u32, 2, c as u32).unwrap();
        }
        assert_eq!(run(&mut e, "crt_wcslen", &[0x10500]), 4);
        run(&mut e, "crt_wcscpy", &[0x10600, 0x10500]);
        assert_eq!(run(&mut e, "crt_wcscmp", &[0x10600, 0x10500]), 0);
    }
}
