//! A sandboxed Windows-style file system for the guest. The guest sees `C:\...` paths;
//! only directories explicitly *mounted* are reachable, and path resolution can never
//! climb out of a mount. Also owns the kernel object handle table (files, searches,
//! events, mutexes, mappings) so handles share one namespace like on Windows.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const INVALID_HANDLE: u32 = 0xFFFF_FFFF;

pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_FILE_EXISTS: u32 = 80;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_NO_MORE_FILES: u32 = 18;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

const GENERIC_WRITE: u32 = 0x4000_0000;

pub struct FindEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub mtime: u64,
}

pub enum Obj {
    File { file: fs::File, path: PathBuf, writable: bool },
    Find { entries: Vec<FindEntry>, pos: usize },
    Event { signaled: bool, manual: bool },
    Mutex { owner: u32, count: u32 },
    Mapping { path: PathBuf, size: u64 },
    /// A thread started by CreateThread: whether it has finished, and its exit code.
    Thread { done: bool, exit_code: u32 },
}

pub struct Vfs {
    /// (normalized Windows prefix like `C:\Game`, host directory)
    mounts: Vec<(String, PathBuf)>,
    /// Current directory as a normalized Windows path.
    pub cwd: String,
    objs: HashMap<u32, Obj>,
    next: u32,
    /// File extensions (lower-case, with the dot) the guest is told do not exist.
    pub hidden_ext: Vec<String>,
}

/// Wildcard match as FindFirstFile does it: `*` and `?`, case-insensitive. `*.*` matches
/// every name (even ones with no dot).
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    if pattern == "*.*" || pattern == "*" {
        return true;
    }
    let (p, n): (Vec<char>, Vec<char>) = (
        pattern.to_lowercase().chars().collect(),
        name.to_lowercase().chars().collect(),
    );
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    go(&p, &n)
}

impl Vfs {
    pub fn new() -> Self {
        Vfs { mounts: Vec::new(), cwd: r"C:\".to_string(), objs: HashMap::new(), next: 0x1000, hidden_ext: Vec::new() }
    }

    /// Make `host` visible to the guest at the Windows path `win_prefix` (e.g. `C:\Game`).
    pub fn mount(&mut self, win_prefix: &str, host: &Path) {
        let prefix = self.normalize(win_prefix);
        self.mounts.retain(|(p, _)| !p.eq_ignore_ascii_case(&prefix));
        self.mounts.push((prefix, host.to_path_buf()));
    }

    /// Absolute, canonical Windows path: backslashes, `.`/`..` resolved, drive upper-cased.
    pub fn normalize(&self, path: &str) -> String {
        let mut p = path.replace('/', "\\");
        for prefix in [r"\\?\", r"\??\"] {
            if let Some(rest) = p.strip_prefix(prefix) {
                p = rest.to_string();
            }
        }
        let (drive, rest) = if p.len() >= 2 && p.as_bytes()[1] == b':' {
            let d = p[..1].to_ascii_uppercase();
            let rest = p[2..].to_string();
            (d, rest)
        } else {
            (self.cwd[..1].to_ascii_uppercase(), p.clone())
        };
        // Relative to the current directory unless it starts with a backslash.
        let base: Vec<String> = if rest.starts_with('\\') {
            Vec::new()
        } else if self.cwd[..1].eq_ignore_ascii_case(&drive) {
            self.cwd[2..].split('\\').filter(|s| !s.is_empty()).map(str::to_string).collect()
        } else {
            Vec::new()
        };
        let mut parts = base;
        for seg in rest.split('\\') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                s => parts.push(s.to_string()),
            }
        }
        format!("{drive}:\\{}", parts.join("\\"))
    }

    /// Map a Windows path to a host path under a mount. Components are matched
    /// case-insensitively against what exists. The final component need not exist.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, u32> {
        let norm = self.normalize(path);
        let lower = norm.to_ascii_lowercase();
        if self.hidden_ext.iter().any(|e| lower.ends_with(e.as_str())) {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        let (prefix, host) = self
            .mounts
            .iter()
            .filter(|(p, _)| {
                let pl = p.to_ascii_lowercase();
                lower == pl || lower.starts_with(&format!("{}\\", pl.trim_end_matches('\\')))
            })
            .max_by_key(|(p, _)| p.len())
            .ok_or(ERROR_PATH_NOT_FOUND)?;
        let rest = &norm[prefix.trim_end_matches('\\').len()..];
        let mut cur = host.clone();
        let comps: Vec<&str> = rest.split('\\').filter(|s| !s.is_empty()).collect();
        for (i, comp) in comps.iter().enumerate() {
            let direct = cur.join(comp);
            if direct.exists() {
                cur = direct;
                continue;
            }
            let found = fs::read_dir(&cur)
                .ok()
                .and_then(|rd| rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(comp)))
                .map(|e| e.path());
            match found {
                Some(p) => cur = p,
                None if i + 1 == comps.len() => cur = direct, // a name that does not exist yet
                None => return Err(ERROR_PATH_NOT_FOUND),
            }
        }
        Ok(cur)
    }

    fn add(&mut self, obj: Obj) -> u32 {
        let h = self.next;
        self.next += 4;
        self.objs.insert(h, obj);
        h
    }

    pub fn get(&self, h: u32) -> Option<&Obj> {
        self.objs.get(&h)
    }

    pub fn get_mut(&mut self, h: u32) -> Option<&mut Obj> {
        self.objs.get_mut(&h)
    }

    pub fn close(&mut self, h: u32) -> bool {
        self.objs.remove(&h).is_some()
    }

    // ---- files -----------------------------------------------------------------------

    /// CreateFile. `disposition`: 1 CREATE_NEW, 2 CREATE_ALWAYS, 3 OPEN_EXISTING,
    /// 4 OPEN_ALWAYS, 5 TRUNCATE_EXISTING.
    pub fn open(&mut self, path: &str, access: u32, disposition: u32) -> Result<u32, u32> {
        let host = self.resolve(path)?;
        let writable = access & GENERIC_WRITE != 0;
        let exists = host.exists();
        if host.is_dir() {
            // Directories can be opened for attribute queries only.
            return Err(ERROR_ACCESS_DENIED);
        }
        let mut opts = fs::OpenOptions::new();
        opts.read(true);
        match disposition {
            1 if exists => return Err(ERROR_FILE_EXISTS),
            1 => {
                opts.write(true).create_new(true);
            }
            2 => {
                opts.write(true).create(true).truncate(true);
            }
            3 if !exists => return Err(ERROR_FILE_NOT_FOUND),
            3 => {
                opts.write(writable);
            }
            4 => {
                opts.write(true).create(true);
            }
            5 if !exists => return Err(ERROR_FILE_NOT_FOUND),
            5 => {
                opts.write(true).truncate(true);
            }
            _ => return Err(ERROR_ACCESS_DENIED),
        }
        let file = opts.open(&host).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ERROR_FILE_NOT_FOUND,
            _ => ERROR_ACCESS_DENIED,
        })?;
        Ok(self.add(Obj::File { file, path: host, writable }))
    }

    pub fn read(&mut self, h: u32, n: usize) -> Result<Vec<u8>, u32> {
        let Some(Obj::File { file, .. }) = self.objs.get_mut(&h) else { return Err(ERROR_INVALID_HANDLE) };
        let mut buf = vec![0u8; n];
        let mut got = 0;
        while got < n {
            match file.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(_) => return Err(ERROR_ACCESS_DENIED),
            }
        }
        buf.truncate(got);
        Ok(buf)
    }

    pub fn write(&mut self, h: u32, data: &[u8]) -> Result<usize, u32> {
        let Some(Obj::File { file, writable, .. }) = self.objs.get_mut(&h) else { return Err(ERROR_INVALID_HANDLE) };
        if !*writable {
            return Err(ERROR_ACCESS_DENIED);
        }
        file.write_all(data).map_err(|_| ERROR_ACCESS_DENIED)?;
        Ok(data.len())
    }

    /// SetFilePointer: `method` 0 begin, 1 current, 2 end. Returns the new position.
    pub fn seek(&mut self, h: u32, offset: i64, method: u32) -> Result<u64, u32> {
        let Some(Obj::File { file, .. }) = self.objs.get_mut(&h) else { return Err(ERROR_INVALID_HANDLE) };
        let from = match method {
            0 => SeekFrom::Start(offset.max(0) as u64),
            1 => SeekFrom::Current(offset),
            _ => SeekFrom::End(offset),
        };
        file.seek(from).map_err(|_| ERROR_ACCESS_DENIED)
    }

    pub fn file_size(&self, h: u32) -> Result<u64, u32> {
        match self.objs.get(&h) {
            Some(Obj::File { file, .. }) => file.metadata().map(|m| m.len()).map_err(|_| ERROR_ACCESS_DENIED),
            _ => Err(ERROR_INVALID_HANDLE),
        }
    }

    pub fn set_end_of_file(&mut self, h: u32) -> Result<(), u32> {
        let Some(Obj::File { file, .. }) = self.objs.get_mut(&h) else { return Err(ERROR_INVALID_HANDLE) };
        let pos = file.stream_position().map_err(|_| ERROR_ACCESS_DENIED)?;
        file.set_len(pos).map_err(|_| ERROR_ACCESS_DENIED)
    }

    pub fn file_path(&self, h: u32) -> Option<&Path> {
        match self.objs.get(&h) {
            Some(Obj::File { path, .. }) => Some(path),
            _ => None,
        }
    }

    // ---- paths -----------------------------------------------------------------------

    pub fn attributes(&self, path: &str) -> Option<u32> {
        let host = self.resolve(path).ok()?;
        let meta = fs::metadata(host).ok()?;
        Some(if meta.is_dir() { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL })
    }

    pub fn mkdir(&self, path: &str) -> Result<(), u32> {
        let host = self.resolve(path)?;
        if host.exists() {
            return Err(ERROR_ALREADY_EXISTS);
        }
        fs::create_dir(host).map_err(|_| ERROR_PATH_NOT_FOUND)
    }

    pub fn rmdir(&self, path: &str) -> Result<(), u32> {
        fs::remove_dir(self.resolve(path)?).map_err(|_| ERROR_ACCESS_DENIED)
    }

    pub fn delete(&self, path: &str) -> Result<(), u32> {
        let host = self.resolve(path)?;
        if !host.is_file() {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        fs::remove_file(host).map_err(|_| ERROR_ACCESS_DENIED)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), u32> {
        let (a, b) = (self.resolve(from)?, self.resolve(to)?);
        if !a.exists() {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        if b.exists() {
            return Err(ERROR_ALREADY_EXISTS);
        }
        fs::rename(a, b).map_err(|_| ERROR_ACCESS_DENIED)
    }

    pub fn set_cwd(&mut self, path: &str) -> bool {
        let norm = self.normalize(path);
        if self.resolve(&norm).is_ok_and(|h| h.is_dir()) {
            self.cwd = norm;
            true
        } else {
            false
        }
    }

    // ---- directory searches --------------------------------------------------------------

    fn entry(path: &Path, name: String) -> Option<FindEntry> {
        let meta = fs::metadata(path).ok()?;
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        Some(FindEntry { name, is_dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() }, mtime })
    }

    /// FindFirstFile. Returns the search handle and the first match.
    pub fn find_first(&mut self, pattern: &str) -> Result<(u32, FindEntry), u32> {
        let norm = self.normalize(pattern);
        let (dir, pat) = norm.rsplit_once('\\').ok_or(ERROR_PATH_NOT_FOUND)?;
        let host_dir = self.resolve(if dir.ends_with(':') { &norm[..3] } else { dir })?;
        let mut entries: Vec<FindEntry> = Vec::new();
        if !host_dir.is_dir() {
            return Err(ERROR_PATH_NOT_FOUND);
        }
        if !pat.contains(['*', '?']) {
            // A plain name: at most one match (possibly with different case).
            let host = self.resolve(&norm)?;
            let name = host.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if let Some(e) = Self::entry(&host, name).filter(|_| host.exists()) {
                entries.push(e);
            }
        } else {
            for dot in [".", ".."] {
                if wildcard_match(pat, dot) {
                    if let Some(e) = Self::entry(&host_dir, dot.to_string()) {
                        entries.push(e);
                    }
                }
            }
            let mut found: Vec<FindEntry> = fs::read_dir(&host_dir)
                .map_err(|_| ERROR_PATH_NOT_FOUND)?
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    wildcard_match(pat, &name).then(|| Self::entry(&e.path(), name)).flatten()
                })
                .collect();
            found.sort_by_key(|e| e.name.to_lowercase());
            entries.extend(found);
        }
        if entries.is_empty() {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        let first = entries.remove(0);
        let h = self.add(Obj::Find { entries, pos: 0 });
        Ok((h, first))
    }

    pub fn find_next(&mut self, h: u32) -> Option<FindEntry> {
        let Some(Obj::Find { entries, pos }) = self.objs.get_mut(&h) else { return None };
        let e = entries.get_mut(*pos)?;
        *pos += 1;
        Some(FindEntry { name: std::mem::take(&mut e.name), is_dir: e.is_dir, size: e.size, mtime: e.mtime })
    }

    // ---- synchronization objects ----------------------------------------------------------

    pub fn create_event(&mut self, manual: bool, signaled: bool) -> u32 {
        self.add(Obj::Event { signaled, manual })
    }

    pub fn create_mutex(&mut self, owned_by: u32) -> u32 {
        self.add(Obj::Mutex { owner: owned_by, count: (owned_by != 0) as u32 })
    }

    pub fn create_mapping(&mut self, file_handle: u32, size: u64) -> Result<u32, u32> {
        let Some(Obj::File { path, .. }) = self.objs.get(&file_handle) else { return Err(ERROR_INVALID_HANDLE) };
        let path = path.clone();
        let size = if size != 0 { size } else { fs::metadata(&path).map_err(|_| ERROR_ACCESS_DENIED)?.len() };
        Ok(self.add(Obj::Mapping { path, size }))
    }

    /// A pagefile-backed (anonymous) mapping: zero-filled memory.
    pub fn create_anonymous_mapping(&mut self, size: u64) -> u32 {
        self.add(Obj::Mapping { path: PathBuf::new(), size })
    }

    pub fn create_thread(&mut self) -> u32 {
        self.add(Obj::Thread { done: false, exit_code: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree: <tmp>/Game/{Data/File.TXT, data/sub/x.bin, a.sga} and a sibling secret.
    fn tree(tag: &str) -> (PathBuf, Vfs) {
        let root = std::env::temp_dir().join(format!("xp_vfs_{}_{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Game/Data/sub")).unwrap();
        fs::write(root.join("Game/Data/File.TXT"), b"hello world").unwrap();
        fs::write(root.join("Game/Data/sub/x.bin"), [1, 2, 3]).unwrap();
        fs::write(root.join("Game/a.sga"), vec![7u8; 1000]).unwrap();
        fs::write(root.join("secret.txt"), b"outside the mount").unwrap();
        let mut v = Vfs::new();
        v.mount(r"C:\Game", &root.join("Game"));
        v.cwd = r"C:\Game".into();
        (root, v)
    }

    #[test]
    fn normalize_resolves_dots_slashes_and_relative_paths() {
        let (_, v) = tree("norm");
        assert_eq!(v.normalize(r"c:\game\data\..\a.sga"), r"C:\game\a.sga");
        assert_eq!(v.normalize("data/File.TXT"), r"C:\Game\data\File.TXT");
        assert_eq!(v.normalize(r"\x"), r"C:\x");
        assert_eq!(v.normalize(r"\\?\C:\Game\a"), r"C:\Game\a");
        assert_eq!(v.normalize(r"C:\..\..\a"), r"C:\a", "cannot climb above the drive root");
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let (root, v) = tree("case");
        let host = v.resolve(r"c:\GAME\data\file.txt").unwrap();
        // The path may keep the guest's spelling on a case-insensitive disk; what matters is
        // that it reaches the right file.
        assert_eq!(fs::read(&host).unwrap(), b"hello world");
        assert_eq!(fs::read(v.resolve(r"C:\Game\DATA\SUB\X.BIN").unwrap()).unwrap(), [1, 2, 3]);
        let _ = root;
    }

    #[test]
    fn paths_outside_mounts_are_unreachable() {
        let (_, v) = tree("sandbox");
        assert_eq!(v.resolve(r"C:\secret.txt"), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.resolve(r"D:\Game\a.sga"), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.resolve(r"C:\Game\..\secret.txt"), Err(ERROR_PATH_NOT_FOUND), ".. cannot escape the mount");
        assert_eq!(v.resolve(r"C:\GameExtra\a.sga"), Err(ERROR_PATH_NOT_FOUND), "prefix match is per component");
    }

    #[test]
    fn longest_mount_wins() {
        let (root, mut v) = tree("mounts");
        fs::create_dir_all(root.join("tmpdir")).unwrap();
        v.mount(r"C:\Game\Data", &root.join("tmpdir"));
        fs::write(root.join("tmpdir/over.txt"), b"x").unwrap();
        assert_eq!(v.resolve(r"C:\Game\Data\over.txt").unwrap(), root.join("tmpdir/over.txt"));
        assert_eq!(v.resolve(r"C:\Game\a.sga").unwrap(), root.join("Game/a.sga"));
    }

    #[test]
    fn open_read_seek_size() {
        let (_, mut v) = tree("read");
        let h = v.open("Data\\file.txt", 0x8000_0000, 3).unwrap();
        assert_eq!(v.file_size(h), Ok(11));
        assert_eq!(v.read(h, 5).unwrap(), b"hello");
        assert_eq!(v.seek(h, 6, 0), Ok(6));
        assert_eq!(v.read(h, 100).unwrap(), b"world", "short read at end of file");
        assert_eq!(v.seek(h, -5, 2), Ok(6));
        assert_eq!(v.seek(h, -2, 1), Ok(4));
        assert!(v.close(h));
        assert_eq!(v.read(h, 1), Err(ERROR_INVALID_HANDLE), "closed handles are dead");
    }

    #[test]
    fn dispositions_behave_like_createfile() {
        let (root, mut v) = tree("disp");
        assert_eq!(v.open("nope.txt", 0x8000_0000, 3), Err(ERROR_FILE_NOT_FOUND));
        assert_eq!(v.open("a.sga", 0x4000_0000, 1), Err(ERROR_FILE_EXISTS));
        let h = v.open("new.txt", 0x4000_0000, 2).unwrap();
        v.write(h, b"abc").unwrap();
        v.close(h);
        assert_eq!(fs::read(root.join("Game/new.txt")).unwrap(), b"abc");
        // read-only handles cannot write
        let r = v.open("new.txt", 0x8000_0000, 3).unwrap();
        assert_eq!(v.write(r, b"x"), Err(ERROR_ACCESS_DENIED));
        // OPEN_ALWAYS keeps contents; CREATE_ALWAYS wipes them
        let h = v.open("new.txt", 0x4000_0000, 4).unwrap();
        assert_eq!(v.file_size(h), Ok(3));
        let h2 = v.open("new.txt", 0x4000_0000, 2).unwrap();
        assert_eq!(v.file_size(h2), Ok(0));
        // opening a directory as a file is refused
        assert_eq!(v.open("Data", 0x8000_0000, 3), Err(ERROR_ACCESS_DENIED));
    }

    #[test]
    fn find_first_next_with_wildcards() {
        let (_, mut v) = tree("find");
        let (h, first) = v.find_first(r"C:\Game\Data\*").unwrap();
        let mut names = vec![first.name];
        while let Some(e) = v.find_next(h) {
            names.push(e.name);
        }
        assert_eq!(names, [".", "..", "File.TXT", "sub"]);
        let (_, only) = v.find_first(r"Data\f*.txt").unwrap();
        assert_eq!((only.name.as_str(), only.size, only.is_dir), ("File.TXT", 11, false));
        assert_eq!(v.find_first(r"Data\*.zzz").err(), Some(ERROR_FILE_NOT_FOUND));
        assert_eq!(v.find_first(r"Nothing\*").err(), Some(ERROR_PATH_NOT_FOUND));
        let (_, exact) = v.find_first("a.sga").unwrap();
        assert_eq!(exact.size, 1000);
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.TXT", "file.txt"));
        assert!(wildcard_match("f?le.*", "FILE.bin"));
        assert!(wildcard_match("*.*", "noext"));
        assert!(!wildcard_match("*.txt", "file.txt.bak"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("?", ""));
    }

    #[test]
    fn directory_operations_and_cwd() {
        let (root, mut v) = tree("dirs");
        assert!(v.attributes("Data").is_some_and(|a| a & FILE_ATTRIBUTE_DIRECTORY != 0));
        assert_eq!(v.attributes("a.sga"), Some(FILE_ATTRIBUTE_NORMAL));
        assert_eq!(v.attributes("zzz"), None);
        v.mkdir("made").unwrap();
        assert_eq!(v.mkdir("made"), Err(ERROR_ALREADY_EXISTS));
        assert!(root.join("Game/made").is_dir());
        v.rename("a.sga", "b.sga").unwrap();
        assert!(root.join("Game/b.sga").exists());
        v.delete("b.sga").unwrap();
        assert_eq!(v.delete("b.sga"), Err(ERROR_FILE_NOT_FOUND));
        v.rmdir("made").unwrap();
        assert!(v.set_cwd("Data"));
        assert_eq!(v.cwd, r"C:\Game\Data");
        assert_eq!(v.resolve("File.TXT").unwrap(), root.join("Game/Data/File.TXT"), "relative to the new cwd");
        assert!(!v.set_cwd(r"C:\nowhere"));
    }

    #[test]
    fn mappings_and_other_objects_share_the_handle_space() {
        let (_, mut v) = tree("objs");
        let f = v.open("a.sga", 0x8000_0000, 3).unwrap();
        let m = v.create_mapping(f, 0).unwrap();
        let e = v.create_event(true, false);
        let mx = v.create_mutex(0);
        let t = v.create_thread();
        let all = [f, m, e, mx, t];
        let mut uniq = all.to_vec();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 5, "distinct handles");
        assert!(matches!(v.get(m), Some(Obj::Mapping { size: 1000, .. })));
        assert!(v.create_mapping(e, 0).is_err(), "only files can be mapped");
    }
}
