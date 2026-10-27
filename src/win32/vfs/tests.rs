//! Tests of `vfs`.

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
    assert_eq!(
        v.normalize(r"C:\..\..\a"),
        r"C:\a",
        "cannot climb above the drive root"
    );
}

#[test]
fn lookup_is_case_insensitive() {
    let (root, v) = tree("case");
    let host = v.resolve(r"c:\GAME\data\file.txt").unwrap();
    // The path may keep the guest's spelling on a case-insensitive disk; what matters is
    // that it reaches the right file.
    assert_eq!(fs::read(&host).unwrap(), b"hello world");
    assert_eq!(
        fs::read(v.resolve(r"C:\Game\DATA\SUB\X.BIN").unwrap()).unwrap(),
        [1, 2, 3]
    );
    let _ = root;
}

#[test]
fn paths_outside_mounts_are_unreachable() {
    let (_, v) = tree("sandbox");
    assert_eq!(v.resolve(r"C:\secret.txt"), Err(ERROR_PATH_NOT_FOUND));
    assert_eq!(v.resolve(r"D:\Game\a.sga"), Err(ERROR_PATH_NOT_FOUND));
    assert_eq!(
        v.resolve(r"C:\Game\..\secret.txt"),
        Err(ERROR_PATH_NOT_FOUND),
        ".. cannot escape the mount"
    );
    assert_eq!(
        v.resolve(r"C:\GameExtra\a.sga"),
        Err(ERROR_PATH_NOT_FOUND),
        "prefix match is per component"
    );
}

#[test]
fn longest_mount_wins() {
    let (root, mut v) = tree("mounts");
    fs::create_dir_all(root.join("tmpdir")).unwrap();
    v.mount(r"C:\Game\Data", &root.join("tmpdir"));
    fs::write(root.join("tmpdir/over.txt"), b"x").unwrap();
    assert_eq!(
        v.resolve(r"C:\Game\Data\over.txt").unwrap(),
        root.join("tmpdir/over.txt")
    );
    assert_eq!(
        v.resolve(r"C:\Game\a.sga").unwrap(),
        root.join("Game/a.sga")
    );
}

#[test]
fn open_read_seek_size() {
    let (_, mut v) = tree("read");
    let h = v.open("Data\\file.txt", 0x8000_0000, 3).unwrap();
    assert_eq!(v.file_size(h), Ok(11));
    assert_eq!(v.read(h, 5).unwrap(), b"hello");
    assert_eq!(v.seek(h, 6, 0), Ok(6));
    assert_eq!(
        v.read(h, 100).unwrap(),
        b"world",
        "short read at end of file"
    );
    assert_eq!(v.seek(h, -5, 2), Ok(6));
    assert_eq!(v.seek(h, -2, 1), Ok(4));
    assert!(v.close(h));
    assert_eq!(
        v.read(h, 1),
        Err(ERROR_INVALID_HANDLE),
        "closed handles are dead"
    );
}

#[test]
fn dispositions_behave_like_createfile() {
    let (root, mut v) = tree("disp");
    assert_eq!(
        v.open("nope.txt", 0x8000_0000, 3),
        Err(ERROR_FILE_NOT_FOUND)
    );
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
    assert_eq!(
        (only.name.as_str(), only.size, only.is_dir),
        ("File.TXT", 11, false)
    );
    assert_eq!(
        v.find_first(r"Data\*.zzz").err(),
        Some(ERROR_FILE_NOT_FOUND)
    );
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
    assert!(
        v.attributes("Data")
            .is_some_and(|a| a & FILE_ATTRIBUTE_DIRECTORY != 0)
    );
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
    assert_eq!(
        v.resolve("File.TXT").unwrap(),
        root.join("Game/Data/File.TXT"),
        "relative to the new cwd"
    );
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
