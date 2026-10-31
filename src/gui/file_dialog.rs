//! GetOpenFileName and GetSaveFileName, through the Mac's own file panels. The program waits
//! while the panel is up, as it would for the Windows dialog. Only files the guest can reach
//! can be chosen, since the answer has to be a path it can use.
//!
//! `XP_FILE_DIALOG=<Windows path>` answers every dialog with that path, for scripted runs.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cpu::{Emu, Res};
use crate::win32::Args;

// OPENFILENAMEA
const FILTER: u32 = 12;
const FILE: u32 = 28;
const MAX_FILE: u32 = 32;
const FILE_TITLE: u32 = 36;
const MAX_FILE_TITLE: u32 = 40;
const INITIAL_DIR: u32 = 44;
const TITLE: u32 = 48;
const FILE_OFFSET: u32 = 56;
const FILE_EXTENSION: u32 = 58;
const DEF_EXT: u32 = 60;

fn string_field(e: &Emu, ofn: u32, field: u32) -> Res<Option<String>> {
    let p = e.mem.read(ofn + field, 4)?;
    if p == 0 {
        return Ok(None);
    }
    Ok(Some(e.mem.read_cstr(p)?).filter(|s| !s.is_empty()))
}

/// The extensions of the dialog's first filter ("Saved games\0*.SV4;*.SC4\0..."): `SV4`, `SC4`.
fn filter_extensions(e: &Emu, ofn: u32) -> Res<Vec<String>> {
    let p = e.mem.read(ofn + FILTER, 4)?;
    if p == 0 {
        return Ok(Vec::new());
    }
    let description = e.mem.read_cstr(p)?;
    let patterns = e.mem.read_cstr(p + description.len() as u32 + 1)?;
    Ok(patterns
        .split(';')
        .filter_map(|f| f.trim().strip_prefix("*."))
        .filter(|x| !x.is_empty() && !x.contains(['*', '?']))
        .map(str::to_string)
        .collect())
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Ask with a Mac file panel; the chosen file's path, or None if cancelled.
fn ask_host(save: bool, title: &str, dir: &Path, name: &str, types: &[String]) -> Option<PathBuf> {
    let mut chooser = format!(
        "choose {} with prompt {} default location POSIX file {}",
        if save { "file name" } else { "file" },
        quoted(title),
        quoted(&dir.to_string_lossy()),
    );
    if save && !name.is_empty() {
        chooser += &format!(" default name {}", quoted(name));
    }
    if !save && !types.is_empty() {
        let list: Vec<String> = types
            .iter()
            .flat_map(|t| [t.to_uppercase(), t.to_lowercase()])
            .map(|t| quoted(&t))
            .collect();
        chooser += &format!(" of type {{{}}}", list.join(", "));
    }
    let out = Command::new("osascript")
        .args(["-e", &format!("POSIX path of ({chooser})")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // cancelled
    }
    let path = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    Some(PathBuf::from(path))
}

/// Fill in the structure with the chosen path. False if the buffer is too small for it.
fn answer(e: &mut Emu, ofn: u32, path: &str) -> Res<bool> {
    let (buffer, max) = (e.mem.read(ofn + FILE, 4)?, e.mem.read(ofn + MAX_FILE, 4)?);
    if path.len() as u32 >= max {
        return Ok(false);
    }
    let mut z = path.as_bytes().to_vec();
    z.push(0);
    e.mem.write_bytes(buffer, &z)?;
    let name_at = path.rfind('\\').map_or(0, |i| i + 1);
    let name = &path[name_at..];
    let ext_at = name.rfind('.').map_or(path.len(), |i| name_at + i + 1);
    e.mem.write(ofn + FILE_OFFSET, 2, name_at as u32)?;
    e.mem.write(ofn + FILE_EXTENSION, 2, ext_at as u32)?;
    let title = e.mem.read(ofn + FILE_TITLE, 4)?;
    if title != 0 && (name.len() as u32) < e.mem.read(ofn + MAX_FILE_TITLE, 4)? {
        let mut z = name.as_bytes().to_vec();
        z.push(0);
        e.mem.write_bytes(title, &z)?;
    }
    Ok(true)
}

/// GetOpenFileNameA (`save` false) and GetSaveFileNameA.
pub fn file_dialog(e: &mut Emu, a: &Args, save: bool) -> Res<u32> {
    let ofn = a[0];
    let chosen = if let Ok(path) = std::env::var("XP_FILE_DIALOG") {
        Some(path)
    } else {
        let start = string_field(e, ofn, INITIAL_DIR)?.unwrap_or_else(|| e.win.vfs.cwd.clone());
        let current = string_field(e, ofn, FILE)?.unwrap_or_default();
        let name = current.rsplit('\\').next().unwrap_or("").to_string();
        // What the buffer holds may be a pattern rather than a name.
        let name = if name.contains(['*', '?']) {
            String::new()
        } else {
            name
        };
        let dir = e.win.vfs.resolve(&start).ok().filter(|d| d.is_dir());
        let dir = dir.or_else(|| e.win.vfs.resolve(&e.win.vfs.cwd).ok());
        let Some(dir) = dir.and_then(|d| d.canonicalize().ok()) else {
            return Ok(0);
        };
        let title = string_field(e, ofn, TITLE)?;
        let title = title.unwrap_or_else(|| if save { "Save".into() } else { "Open".into() });
        let types = filter_extensions(e, ofn)?;
        let host = ask_host(save, &title, &dir, &name, &types);
        let guest = host.as_deref().map(|h| (h, e.win.vfs.guest_path(h)));
        match guest {
            None => None,
            Some((_, Some(guest))) => Some(guest),
            Some((h, None)) => {
                eprintln!("[file dialog] {} is outside the game's folder", h.display());
                None
            }
        }
    };
    let Some(mut path) = chosen else {
        return Ok(0);
    };
    if let Some(ext) = string_field(e, ofn, DEF_EXT)?
        && !path.rsplit('\\').next().unwrap_or("").contains('.')
    {
        path = format!("{path}.{ext}");
    }
    Ok(answer(e, ofn, &path)? as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_fills_the_path_and_where_its_parts_start() {
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x1000);
        let ofn = 0x10000;
        e.mem.write(ofn + FILE, 4, 0x10100).unwrap();
        e.mem.write(ofn + MAX_FILE, 4, 260).unwrap();
        e.mem.write(ofn + FILE_TITLE, 4, 0x10200).unwrap();
        e.mem.write(ofn + MAX_FILE_TITLE, 4, 64).unwrap();
        assert!(answer(&mut e, ofn, r"C:\Game\Saved Games\a.SV4").unwrap());
        assert_eq!(
            e.mem.read_cstr(0x10100).unwrap(),
            r"C:\Game\Saved Games\a.SV4"
        );
        assert_eq!(e.mem.read_cstr(0x10200).unwrap(), "a.SV4");
        assert_eq!(e.mem.read(ofn + FILE_OFFSET, 2).unwrap(), 20);
        assert_eq!(e.mem.read(ofn + FILE_EXTENSION, 2).unwrap(), 22);
        e.mem.write(ofn + MAX_FILE, 4, 4).unwrap();
        assert!(!answer(&mut e, ofn, r"C:\long").unwrap());
    }

    #[test]
    fn extensions_come_from_the_first_filter() {
        let mut e = Emu::new();
        e.mem.map(0x10000, 0x1000);
        e.mem.write(0x10000 + FILTER, 4, 0x10100).unwrap();
        e.mem
            .write_bytes(0x10100, b"Saved\0*.SV4;*.sc4\0All\0*.*\0\0")
            .unwrap();
        assert_eq!(filter_extensions(&e, 0x10000).unwrap(), ["SV4", "sc4"]);
    }
}
