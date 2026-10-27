//! The registry, in memory: keys hold typed values; nothing is persisted between runs.
//! It starts empty, apart from what a game's installer would have written (seeded by
//! `game_optimizations::setup`).

use std::collections::HashMap;

use crate::cpu::{Emu, Res};
use crate::win32::{Args, Handler};

const ERROR_SUCCESS: u32 = 0;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_MORE_DATA: u32 = 234;
const ERROR_NO_MORE_ITEMS: u32 = 259;

pub const REG_SZ: u32 = 1;

#[derive(Default)]
pub struct Registry {
    /// Keys by full lower-case path ("hklm\\software\\..."): their values by lower-case name,
    /// each with the name as written, the type and the data.
    keys: HashMap<String, HashMap<String, (String, u32, Vec<u8>)>>,
    /// Open handles and the key path each refers to.
    handles: HashMap<u32, String>,
    next_handle: u32,
}

fn root_name(hkey: u32) -> Option<&'static str> {
    Some(match hkey {
        0x8000_0000 => "hkcr",
        0x8000_0001 => "hkcu",
        0x8000_0002 => "hklm",
        0x8000_0003 => "hku",
        0x8000_0005 => "hkcc",
        _ => return None,
    })
}

impl Registry {
    fn path_of(&self, hkey: u32, sub: &str) -> Option<String> {
        let base = match root_name(hkey) {
            Some(r) => r.to_string(),
            None => self.handles.get(&hkey)?.clone(),
        };
        let sub = sub.trim_matches('\\').to_ascii_lowercase();
        Some(if sub.is_empty() {
            base
        } else {
            format!("{base}\\{sub}")
        })
    }

    fn open(&mut self, path: String) -> u32 {
        self.next_handle += 1;
        let h = 0x0052_0000 + self.next_handle;
        self.handles.insert(h, path);
        h
    }

    /// Create a key (and its parents) if need be.
    fn create(&mut self, path: &str) {
        let mut at = String::new();
        for part in path.split('\\') {
            if !at.is_empty() {
                at.push('\\');
            }
            at.push_str(part);
            self.keys.entry(at.clone()).or_default();
        }
    }

    /// Store a value, creating its key: `key` is a full path such as
    /// `HKEY_LOCAL_MACHINE\Software\Vendor\Product`.
    pub fn set(&mut self, key: &str, name: &str, kind: u32, data: Vec<u8>) {
        let (root, rest) = key.split_once('\\').unwrap_or((key, ""));
        let root = match root.to_ascii_uppercase().as_str() {
            "HKEY_CLASSES_ROOT" | "HKCR" => "hkcr",
            "HKEY_CURRENT_USER" | "HKCU" => "hkcu",
            "HKEY_USERS" | "HKU" => "hku",
            _ => "hklm",
        };
        let path = format!("{root}\\{}", rest.to_ascii_lowercase());
        self.create(&path);
        self.keys
            .get_mut(&path)
            .unwrap()
            .insert(name.to_ascii_lowercase(), (name.to_string(), kind, data));
    }

    /// A string value: its bytes with the terminating NUL.
    pub fn set_str(&mut self, key: &str, name: &str, value: &str) {
        let mut data = value.as_bytes().to_vec();
        data.push(0);
        self.set(key, name, REG_SZ, data);
    }
}

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "advapi32" {
        return None;
    }
    let r: (usize, Handler) = match name {
        "RegOpenKeyA" => (3, |e, a| open_key(e, a[0], a[1], a[2], false)),
        "RegOpenKeyExA" => (5, |e, a| open_key(e, a[0], a[1], a[4], false)),
        "RegCreateKeyA" => (3, |e, a| open_key(e, a[0], a[1], a[2], true)),
        "RegCreateKeyExA" => (9, |e, a| {
            let existed = {
                let sub = if a[1] != 0 {
                    e.mem.read_cstr(a[1])?
                } else {
                    String::new()
                };
                e.win
                    .registry
                    .path_of(a[0], &sub)
                    .is_some_and(|p| e.win.registry.keys.contains_key(&p))
            };
            let r = open_key(e, a[0], a[1], a[7], true)?;
            if r == ERROR_SUCCESS && a[8] != 0 {
                e.mem.write(a[8], 4, if existed { 2 } else { 1 })?; // REG_OPENED_EXISTING_KEY / REG_CREATED_NEW_KEY
            }
            Ok(r)
        }),
        "RegCloseKey" => (1, |e, a| {
            e.win.registry.handles.remove(&a[0]);
            Ok(ERROR_SUCCESS)
        }),
        "RegFlushKey" => (1, |_, _| Ok(ERROR_SUCCESS)),
        "RegQueryValueExA" => (6, query_value),
        "RegSetValueExA" => (6, |e, a| {
            let Some(path) = e.win.registry.path_of(a[0], "") else {
                return Ok(ERROR_INVALID_HANDLE);
            };
            let name = if a[1] != 0 {
                e.mem.read_cstr(a[1])?
            } else {
                String::new()
            };
            let data = e.mem.read_bytes(a[4], a[5])?;
            let key = e.win.registry.keys.entry(path).or_default();
            key.insert(name.to_ascii_lowercase(), (name, a[3], data));
            Ok(ERROR_SUCCESS)
        }),
        "RegDeleteValueA" => (2, |e, a| {
            let Some(path) = e.win.registry.path_of(a[0], "") else {
                return Ok(ERROR_INVALID_HANDLE);
            };
            let name = if a[1] != 0 {
                e.mem.read_cstr(a[1])?
            } else {
                String::new()
            };
            let gone = e
                .win
                .registry
                .keys
                .get_mut(&path)
                .and_then(|k| k.remove(&name.to_ascii_lowercase()));
            Ok(if gone.is_some() {
                ERROR_SUCCESS
            } else {
                ERROR_FILE_NOT_FOUND
            })
        }),
        "RegDeleteKeyA" => (2, |e, a| {
            let sub = e.mem.read_cstr(a[1])?;
            let Some(path) = e.win.registry.path_of(a[0], &sub) else {
                return Ok(ERROR_INVALID_HANDLE);
            };
            Ok(if e.win.registry.keys.remove(&path).is_some() {
                ERROR_SUCCESS
            } else {
                ERROR_FILE_NOT_FOUND
            })
        }),
        "RegEnumValueA" => (8, enum_value),
        _ => return None,
    };
    Some(r)
}

fn open_key(e: &mut Emu, hkey: u32, sub: u32, out: u32, create: bool) -> Res<u32> {
    let sub = if sub != 0 {
        e.mem.read_cstr(sub)?
    } else {
        String::new()
    };
    let reg = &mut e.win.registry;
    let Some(path) = reg.path_of(hkey, &sub) else {
        return Ok(ERROR_INVALID_HANDLE);
    };
    if create {
        reg.create(&path);
    } else if !reg.keys.contains_key(&path) && root_name(hkey).is_none_or(|_| !sub.is_empty()) {
        return Ok(ERROR_FILE_NOT_FOUND);
    }
    let h = reg.open(path);
    e.mem.write(out, 4, h)?;
    Ok(ERROR_SUCCESS)
}

/// Copy value data out as RegQueryValueEx does: the size is always reported, and a buffer
/// too small gets ERROR_MORE_DATA.
fn give_data(
    e: &mut Emu,
    kind: u32,
    data: &[u8],
    kind_out: u32,
    buf: u32,
    size_out: u32,
) -> Res<u32> {
    if kind_out != 0 {
        e.mem.write(kind_out, 4, kind)?;
    }
    if size_out == 0 {
        return Ok(ERROR_SUCCESS);
    }
    let room = e.mem.read(size_out, 4)?;
    e.mem.write(size_out, 4, data.len() as u32)?;
    if buf == 0 {
        return Ok(ERROR_SUCCESS);
    }
    if (room as usize) < data.len() {
        return Ok(ERROR_MORE_DATA);
    }
    e.mem.write_bytes(buf, data)?;
    Ok(ERROR_SUCCESS)
}

fn query_value(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(path) = e.win.registry.path_of(a[0], "") else {
        return Ok(ERROR_INVALID_HANDLE);
    };
    let name = if a[1] != 0 {
        e.mem.read_cstr(a[1])?
    } else {
        String::new()
    };
    let Some((_, kind, data)) = e
        .win
        .registry
        .keys
        .get(&path)
        .and_then(|k| k.get(&name.to_ascii_lowercase()))
        .cloned()
    else {
        return Ok(ERROR_FILE_NOT_FOUND);
    };
    give_data(e, kind, &data, a[3], a[4], a[5])
}

fn enum_value(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(path) = e.win.registry.path_of(a[0], "") else {
        return Ok(ERROR_INVALID_HANDLE);
    };
    let mut values: Vec<(String, u32, Vec<u8>)> = e
        .win
        .registry
        .keys
        .get(&path)
        .map(|k| k.values().cloned().collect())
        .unwrap_or_default();
    values.sort_by(|x, y| x.0.cmp(&y.0));
    let Some((name, kind, data)) = values.into_iter().nth(a[1] as usize) else {
        return Ok(ERROR_NO_MORE_ITEMS);
    };
    let room = e.mem.read(a[3], 4)?;
    if room as usize <= name.len() {
        return Ok(ERROR_MORE_DATA);
    }
    e.mem.write_bytes(a[2], name.as_bytes())?;
    e.mem.write(a[2] + name.len() as u32, 1, 0)?;
    e.mem.write(a[3], 4, name.len() as u32)?;
    give_data(e, kind, &data, a[5], a[6], a[7])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::call;

    #[test]
    fn values_can_be_seeded_opened_and_read_back() {
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x1000);
        e.win.registry.set_str(
            r"HKEY_LOCAL_MACHINE\Software\Vendor\Game",
            "Path",
            r"C:\Game",
        );
        let run = |e: &mut Emu, name: &str, args: &[u32]| {
            call(e, lookup("advapi32", name).unwrap().1, args)
        };
        e.mem
            .write_bytes(0x10000, b"SOFTWARE\\vendor\\game\0Path\0Missing\0")
            .unwrap();
        assert_eq!(
            run(&mut e, "RegOpenKeyA", &[0x8000_0002, 0x10000, 0x10100]),
            ERROR_SUCCESS,
            "case does not matter"
        );
        let hk = e.mem.read(0x10100, 4).unwrap();
        // size query first, then the data
        e.mem.write(0x10104, 4, 0).unwrap();
        assert_eq!(
            run(
                &mut e,
                "RegQueryValueExA",
                &[hk, 0x10015, 0, 0x10108, 0, 0x10104]
            ),
            ERROR_SUCCESS
        );
        assert_eq!(e.mem.read(0x10104, 4).unwrap(), 8);
        assert_eq!(
            run(
                &mut e,
                "RegQueryValueExA",
                &[hk, 0x10015, 0, 0x10108, 0x10200, 0x10104]
            ),
            ERROR_SUCCESS
        );
        assert_eq!(e.mem.read_cstr(0x10200).unwrap(), r"C:\Game");
        assert_eq!(e.mem.read(0x10108, 4).unwrap(), REG_SZ);
        assert_eq!(
            run(&mut e, "RegQueryValueExA", &[hk, 0x1001a, 0, 0, 0, 0]),
            ERROR_FILE_NOT_FOUND
        );
        // an unknown key is not found; creating it makes it openable
        e.mem.write_bytes(0x10300, b"Software\\Other\0").unwrap();
        assert_eq!(
            run(&mut e, "RegOpenKeyA", &[0x8000_0001, 0x10300, 0x10100]),
            ERROR_FILE_NOT_FOUND
        );
        assert_eq!(
            run(&mut e, "RegCreateKeyA", &[0x8000_0001, 0x10300, 0x10100]),
            ERROR_SUCCESS
        );
        assert_eq!(
            run(&mut e, "RegOpenKeyA", &[0x8000_0001, 0x10300, 0x10100]),
            ERROR_SUCCESS
        );
    }
}
