//! End-to-end: synthetic PE images -> loader -> interpreter -> Win32 layer.

use crate::cpu::{Emu, Stop};
use crate::loader;
use crate::loader::pe::Pe;
use crate::tests::util::{build_pe, CODE_VA};

fn call_iat(code: &mut Vec<u8>, slot: u32) {
    code.extend_from_slice(&[0xFF, 0x15]);
    code.extend_from_slice(&slot.to_le_bytes());
}

fn load_and_run(image: &[u8]) -> (Emu, Stop) {
    let mut emu = Emu::new();
    loader::load(&mut emu, image).expect("image loads");
    let stop = emu.run(100_000);
    (emu, stop)
}

#[test]
fn builder_output_parses() {
    let image = build_pe(&[("KERNEL32.dll", &["ExitProcess", "GetVersion"])], |_| vec![0xC3]);
    let pe = Pe::parse(&image).unwrap();
    assert_eq!(pe.entry_rva, 0x1000);
    assert_eq!(pe.imports.len(), 1);
    assert_eq!(pe.imports[0].dll, "KERNEL32.dll");
    assert_eq!(pe.imports[0].functions, ["ExitProcess", "GetVersion"]);
}

#[test]
fn exit_process_through_iat() {
    let image = build_pe(&[("KERNEL32.dll", &["ExitProcess"])], |l| {
        let mut c = vec![0x6A, 42]; // push 42
        call_iat(&mut c, l.iat_va(0, 0));
        c
    });
    let (_, stop) = load_and_run(&image);
    assert_eq!(stop, Stop::Exit(42));
}

#[test]
fn returning_from_entry_exits_with_eax() {
    let image = build_pe(&[], |_| vec![0xB8, 7, 0, 0, 0, 0xC3]);
    let (_, stop) = load_and_run(&image);
    assert_eq!(stop, Stop::Exit(7));
}

#[test]
fn write_file_to_stdout() {
    let image = build_pe(
        &[("KERNEL32.dll", &["GetStdHandle", "WriteFile", "ExitProcess"])],
        |l| {
            let text = CODE_VA + 0x40;
            let written = CODE_VA + 0x50;
            let mut c = vec![0x6A, 0xF5]; // push -11 (STD_OUTPUT_HANDLE)
            call_iat(&mut c, l.iat_va(0, 0));
            c.extend_from_slice(&[0x6A, 0x00]); // lpOverlapped
            c.push(0x68);
            c.extend_from_slice(&written.to_le_bytes());
            c.extend_from_slice(&[0x6A, 5]); // length
            c.push(0x68);
            c.extend_from_slice(&text.to_le_bytes());
            c.push(0x50); // push eax (handle)
            call_iat(&mut c, l.iat_va(0, 1));
            c.extend_from_slice(&[0x6A, 0]);
            call_iat(&mut c, l.iat_va(0, 2));
            assert!(c.len() < 0x40);
            c.resize(0x40, 0x90);
            c.extend_from_slice(b"hello");
            c.resize(0x54, 0);
            c
        },
    );
    let (emu, stop) = load_and_run(&image);
    assert_eq!(stop, Stop::Exit(0));
    assert_eq!(emu.win.stdout, b"hello");
    assert_eq!(emu.mem.read(CODE_VA + 0x50, 4).unwrap(), 5, "bytes-written out-param");
}

#[test]
fn heap_alloc_returns_usable_memory() {
    let image = build_pe(&[("KERNEL32.dll", &["GetProcessHeap", "HeapAlloc", "ExitProcess"])], |l| {
        let mut c = Vec::new();
        call_iat(&mut c, l.iat_va(0, 0));
        c.extend_from_slice(&[0x6A, 100, 0x6A, 0, 0x50]); // push 100; push 0; push eax
        call_iat(&mut c, l.iat_va(0, 1));
        c.extend_from_slice(&[0xC7, 0x00, 0x34, 0x12, 0, 0]); // mov dword [eax], 0x1234
        c.extend_from_slice(&[0x8B, 0x08]); // mov ecx, [eax]
        c.push(0x51); // push ecx
        call_iat(&mut c, l.iat_va(0, 2));
        c
    });
    let (_, stop) = load_and_run(&image);
    assert_eq!(stop, Stop::Exit(0x1234));
}

#[test]
fn teb_is_reachable_through_fs() {
    // mov eax, fs:[0x18] (TEB self pointer); ret -> exit code
    let image = build_pe(&[], |_| vec![0x64, 0xA1, 0x18, 0, 0, 0, 0xC3]);
    let (_, stop) = load_and_run(&image);
    assert_eq!(stop, Stop::Exit(crate::win32::TEB_ADDR));
}

#[test]
fn reports_xp_version() {
    let image = build_pe(&[("KERNEL32.dll", &["GetVersion"])], |l| {
        let mut c = Vec::new();
        call_iat(&mut c, l.iat_va(0, 0));
        c.push(0xC3);
        c
    });
    let (_, stop) = load_and_run(&image);
    let Stop::Exit(v) = stop else { panic!("{stop:?}") };
    assert_eq!((v & 0xff, (v >> 8) & 0xff, v >> 16), (5, 1, 2600));
}

#[test]
fn get_proc_address_resolves_supported_and_rejects_unknown() {
    let image = build_pe(
        &[("KERNEL32.dll", &["GetModuleHandleA", "GetProcAddress", "ExitProcess"])],
        |l| {
            let mod_name = CODE_VA + 0x80;
            let good = CODE_VA + 0x90;
            let bad = CODE_VA + 0xA0;
            let mut c = Vec::new();
            // h = GetModuleHandleA("kernel32.dll")
            c.push(0x68);
            c.extend_from_slice(&mod_name.to_le_bytes());
            call_iat(&mut c, l.iat_va(0, 0));
            c.push(0x89);
            c.push(0xC3); // mov ebx, eax
            // eax = GetProcAddress(h, "GetVersion"); esi = eax
            c.push(0x68);
            c.extend_from_slice(&good.to_le_bytes());
            c.push(0x53);
            call_iat(&mut c, l.iat_va(0, 1));
            c.extend_from_slice(&[0x89, 0xC6]); // mov esi, eax
            // eax = GetProcAddress(h, "NoSuchApi")
            c.push(0x68);
            c.extend_from_slice(&bad.to_le_bytes());
            c.push(0x53);
            call_iat(&mut c, l.iat_va(0, 1));
            // call esi (GetVersion) -> eax = version; then push eax; ExitProcess(version)
            c.extend_from_slice(&[0xFF, 0xD6, 0x50]);
            call_iat(&mut c, l.iat_va(0, 2));
            assert!(c.len() < 0x80);
            c.resize(0x80, 0x90);
            c.extend_from_slice(b"kernel32.dll\0\0\0\0");
            c.extend_from_slice(b"GetVersion\0\0\0\0\0\0");
            c.extend_from_slice(b"NoSuchApi\0");
            c
        },
    );
    let (emu, stop) = load_and_run(&image);
    assert_eq!(stop, Stop::Exit(0x0A28_0105));
    assert_eq!(emu.regs[3] & 0xffff_0000, 0x7800_0000, "module handle is fake but stable");
}

#[test]
fn unimplemented_import_stops_with_its_name() {
    let image = build_pe(&[("KERNEL32.dll", &["NoSuchFunction"])], |l| {
        let mut c = Vec::new();
        call_iat(&mut c, l.iat_va(0, 0));
        c
    });
    let mut emu = Emu::new();
    let img = loader::load(&mut emu, &image).unwrap();
    assert_eq!(img.unsupported, ["KERNEL32.dll!NoSuchFunction"]);
    assert_eq!(emu.run(100), Stop::Unimplemented("kernel32!NoSuchFunction".into()));
}

#[test]
fn message_box_is_captured() {
    let image = build_pe(&[("USER32.dll", &["MessageBoxA"])], |l| {
        let text = CODE_VA + 0x40;
        let title = CODE_VA + 0x50;
        let mut c = vec![0x6A, 0]; // uType
        c.push(0x68);
        c.extend_from_slice(&title.to_le_bytes());
        c.push(0x68);
        c.extend_from_slice(&text.to_le_bytes());
        c.extend_from_slice(&[0x6A, 0]); // hWnd
        call_iat(&mut c, l.iat_va(0, 0));
        c.push(0xC3);
        c.resize(0x40, 0x90);
        c.extend_from_slice(b"Hi there\0\0\0\0\0\0\0\0");
        c.extend_from_slice(b"Title\0");
        c
    });
    let (emu, stop) = load_and_run(&image);
    assert_eq!(stop, Stop::Exit(1), "MessageBox returns IDOK");
    assert_eq!(emu.win.messages, ["Title: Hi there"]);
}

#[test]
fn bad_images_are_rejected() {
    let mut emu = Emu::new();
    assert!(loader::load(&mut emu, b"not a pe").is_err());
    let mut image = build_pe(&[], |_| vec![0xC3]);
    image[0x40 + 4] = 0x64; // machine -> x64
    image[0x40 + 5] = 0x86;
    assert!(loader::load(&mut Emu::new(), &image).is_err());
}

// ---- DLLs, relocation, DllMain ------------------------------------------------------------

mod dll {
    use super::*;
    use crate::tests::util::{build, Spec};

    const DLL_BASE: u32 = 0x1000_0000;
    const DATA: u32 = 0x40; // code-section offset used as the DLL's global variable

    /// DllMain stores 1 into DATA; GetFlag returns it; GetDataAddr returns &DATA.
    /// The absolute addresses are relocated if the DLL has to move.
    fn flag_dll(base: u32) -> Vec<u8> {
        let spec = Spec {
            exports: &[("GetFlag", 0x20), ("GetDataAddr", 0x30)],
            relocs: &[2, 0x21, 0x31],
            dll: true,
            base,
            ..Spec::default()
        };
        build(&spec, |l| {
            let data = l.code_va(DATA).to_le_bytes();
            let mut c = vec![0xC7, 0x05]; // mov dword [DATA], 1
            c.extend_from_slice(&data);
            c.extend_from_slice(&[1, 0, 0, 0]);
            c.extend_from_slice(&[0xB8, 1, 0, 0, 0, 0xC2, 12, 0]); // mov eax,1; ret 12
            c.resize(0x20, 0x90);
            c.push(0xA1); // GetFlag: mov eax, [DATA]
            c.extend_from_slice(&data);
            c.push(0xC3);
            c.resize(0x30, 0x90);
            c.push(0xB8); // GetDataAddr: mov eax, DATA
            c.extend_from_slice(&data);
            c.push(0xC3);
            c.resize(0x44, 0);
            c
        })
    }

    fn export(emu: &Emu, dll: &str, name: &str) -> u32 {
        emu.win.find_module(dll).unwrap().exports.as_ref().unwrap().by_name[name]
    }

    #[test]
    fn dll_main_runs_and_exports_resolve() {
        let mut emu = Emu::new();
        emu.win.virtual_files.insert("flag.dll".into(), flag_dll(DLL_BASE));
        let h = emu.load_library("flag.dll").unwrap();
        assert_eq!(h, DLL_BASE, "loaded at its preferred base");
        let get_flag = export(&emu, "flag", "GetFlag");
        assert_eq!(emu.call_function(get_flag, &[]).unwrap(), 1, "DllMain(PROCESS_ATTACH) ran");
    }

    #[test]
    fn load_library_is_idempotent() {
        let mut emu = Emu::new();
        emu.win.virtual_files.insert("flag.dll".into(), flag_dll(DLL_BASE));
        let a = emu.load_library("FLAG.DLL").unwrap();
        let b = emu.load_library("flag").unwrap();
        assert_eq!(a, b);
        assert_eq!(emu.win.modules.len(), 1);
    }

    #[test]
    fn colliding_dlls_are_relocated() {
        let mut emu = Emu::new();
        emu.win.virtual_files.insert("a.dll".into(), flag_dll(DLL_BASE));
        emu.win.virtual_files.insert("b.dll".into(), flag_dll(DLL_BASE));
        let a = emu.load_library("a.dll").unwrap();
        let b = emu.load_library("b.dll").unwrap();
        assert_eq!(a, DLL_BASE);
        assert_ne!(b, DLL_BASE, "second DLL had to move");

        for (name, base) in [("a", a), ("b", b)] {
            let addr = emu.call_function(export(&emu, name, "GetDataAddr"), &[]).unwrap();
            assert_eq!(addr, base + 0x1000 + DATA, "{name}: absolute address was relocated");
            assert_eq!(emu.call_function(export(&emu, name, "GetFlag"), &[]).unwrap(), 1);
        }
    }

    #[test]
    fn dll_data_is_independent_per_instance() {
        let mut emu = Emu::new();
        emu.win.virtual_files.insert("a.dll".into(), flag_dll(DLL_BASE));
        emu.win.virtual_files.insert("b.dll".into(), flag_dll(DLL_BASE));
        let a = emu.load_library("a.dll").unwrap();
        emu.load_library("b.dll").unwrap();
        emu.mem.write(a + 0x1000 + DATA, 4, 99).unwrap();
        assert_eq!(emu.call_function(export(&emu, "a", "GetFlag"), &[]).unwrap(), 99);
        assert_eq!(emu.call_function(export(&emu, "b", "GetFlag"), &[]).unwrap(), 1);
    }

    #[test]
    fn exe_imports_from_real_dll() {
        let lib = build(
            &Spec { exports: &[("Answer", 0)], dll: true, base: DLL_BASE, ..Spec::default() },
            |_| vec![0xB8, 0xD2, 0x04, 0, 0, 0xC3], // mov eax, 1234; ret
        );
        let exe = build_pe(&[("lib.dll", &["Answer"])], |l| {
            let mut c = Vec::new();
            call_iat(&mut c, l.iat_va(0, 0));
            c.push(0xC3); // return Answer() as the exit code
            c
        });
        let mut emu = Emu::new();
        emu.win.virtual_files.insert("lib.dll".into(), lib);
        let img = loader::load(&mut emu, &exe).unwrap();
        assert!(img.unsupported.is_empty());
        assert_eq!(emu.run(1000), Stop::Exit(1234));
    }

    #[test]
    fn guest_load_library_and_get_proc_address() {
        let lib = flag_dll(DLL_BASE);
        let exe = build_pe(&[("KERNEL32.dll", &["LoadLibraryA", "GetProcAddress", "ExitProcess"])], |l| {
            let (lib_name, fn_name) = (l.code_va(0x80), l.code_va(0x90));
            let mut c = vec![0x68];
            c.extend_from_slice(&lib_name.to_le_bytes());
            call_iat(&mut c, l.iat_va(0, 0)); // eax = LoadLibraryA("flag.dll")
            c.push(0x68);
            c.extend_from_slice(&fn_name.to_le_bytes());
            c.push(0x50);
            call_iat(&mut c, l.iat_va(0, 1)); // eax = GetProcAddress(h, "GetFlag")
            c.extend_from_slice(&[0xFF, 0xD0, 0x50]); // call eax; push eax
            call_iat(&mut c, l.iat_va(0, 2)); // ExitProcess(flag)
            assert!(c.len() < 0x80);
            c.resize(0x80, 0x90);
            c.extend_from_slice(b"flag.dll\0\0\0\0\0\0\0\0");
            c.extend_from_slice(b"GetFlag\0");
            c
        });
        let mut emu = Emu::new();
        emu.win.virtual_files.insert("flag.dll".into(), lib);
        loader::load(&mut emu, &exe).unwrap();
        assert_eq!(emu.run(10_000), Stop::Exit(1), "DllMain ran inside LoadLibraryA");
    }

    #[test]
    fn missing_dll_is_null_and_sets_last_error() {
        let exe = build_pe(&[("KERNEL32.dll", &["LoadLibraryA", "GetLastError"])], |l| {
            let name = l.code_va(0x40);
            let mut c = vec![0x68];
            c.extend_from_slice(&name.to_le_bytes());
            call_iat(&mut c, l.iat_va(0, 0));
            c.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
            call_iat(&mut c, l.iat_va(0, 1)); // eax = GetLastError()
            c.push(0xC3);
            c.resize(0x40, 0x90);
            c.extend_from_slice(b"nothere.dll\0");
            c
        });
        let mut emu = Emu::new();
        loader::load(&mut emu, &exe).unwrap();
        assert_eq!(emu.run(1000), Stop::Exit(126));
        assert_eq!(emu.regs[3], 0);
    }

    #[test]
    fn moved_image_without_relocations_still_loads_for_its_resources() {
        // No relocation table, and its preferred base is taken: resource-only DLLs do not care.
        let no_relocs = build(&Spec { dll: true, base: DLL_BASE, ..Spec::default() }, |_| vec![0xC3]);
        let mut emu = Emu::new();
        emu.mem.map(DLL_BASE, 0x3000); // something already lives there
        emu.win.virtual_files.insert("x.dll".into(), no_relocs);
        let h = emu.load_library("x.dll").unwrap();
        assert_ne!(h, 0);
        assert_ne!(h, DLL_BASE, "it was placed elsewhere");
    }

    /// DllMain(hinst, reason, reserved) writes `reason` to a fixed, unrelocated address
    /// (0x3000_0000) and bumps a call counter at 0x3000_0004, then returns TRUE.
    fn reason_dll() -> Vec<u8> {
        let spec = Spec { dll: true, base: DLL_BASE, relocs: &[], ..Spec::default() };
        build(&spec, |_| {
            let mut c = vec![0x8B, 0x44, 0x24, 0x08, 0xA3]; // mov eax,[esp+8]; mov [0x30000000],eax
            c.extend_from_slice(&0x3000_0000u32.to_le_bytes());
            c.extend_from_slice(&[0xFF, 0x05]); // inc dword [0x30000004]
            c.extend_from_slice(&0x3000_0004u32.to_le_bytes());
            c.extend_from_slice(&[0xB8, 1, 0, 0, 0, 0xC2, 12, 0]);
            c
        })
    }

    fn reason_state(emu: &Emu) -> (u32, u32) {
        (emu.mem.read(0x3000_0000, 4).unwrap(), emu.mem.read(0x3000_0004, 4).unwrap())
    }

    #[test]
    fn free_library_counts_references_and_unloads_at_zero() {
        let mut emu = Emu::new();
        emu.mem.map(0x3000_0000, 0x1000);
        emu.win.virtual_files.insert("rc.dll".into(), reason_dll());
        let h = emu.load_library("rc.dll").unwrap();
        assert_eq!(reason_state(&emu), (1, 1), "DLL_PROCESS_ATTACH ran once");
        assert_eq!(emu.load_library("RC.DLL").unwrap(), h, "already loaded: same module");
        assert_eq!(reason_state(&emu).1, 1, "...and DllMain did not run again");

        assert!(emu.free_library(h).unwrap());
        assert!(emu.mem.is_mapped(h), "one reference left: still loaded");
        assert_eq!(reason_state(&emu), (1, 1));

        assert!(emu.free_library(h).unwrap());
        assert_eq!(reason_state(&emu), (0, 2), "DLL_PROCESS_DETACH ran when the count hit zero");
        assert!(!emu.mem.is_mapped(h), "its memory is released");
        assert!(emu.win.find_module("rc.dll").is_none());
        assert!(!emu.free_library(h).unwrap(), "freeing a handle that is not loaded reports failure");
    }

    #[test]
    fn reloading_after_free_starts_the_dll_from_scratch() {
        // This is what a game that probes a plugin DLL (load, look up exports, free) relies on:
        // the second load must run DllMain and the static constructors again, not reuse stale state.
        let mut emu = Emu::new();
        emu.mem.map(0x3000_0000, 0x1000);
        emu.win.virtual_files.insert("rc.dll".into(), reason_dll());
        let h = emu.load_library("rc.dll").unwrap();
        emu.free_library(h).unwrap();
        let h2 = emu.load_library("rc.dll").unwrap();
        assert_ne!(h2, 0);
        assert_eq!(reason_state(&emu), (1, 3), "attach, detach, attach");
    }

    #[test]
    fn a_module_is_the_same_whether_named_by_path_or_by_file_name() {
        let mut emu = Emu::new();
        emu.mem.map(0x3000_0000, 0x1000);
        emu.win.virtual_files.insert("rc.dll".into(), reason_dll());
        let a = emu.load_library(r"C:\Game\rc.dll").unwrap();
        let b = emu.load_library("rc.dll").unwrap();
        let c = emu.load_library("rc").unwrap();
        assert_eq!((a, b), (b, c), "one module under three spellings");
        assert_eq!(emu.win.modules.len(), 1);
        assert_eq!(emu.win.modules[0].path, r"c:\game\rc.dll", "named by its full guest path, not a mangled one");
        assert_eq!(emu.win.modules[0].refs, 3);
        assert_eq!(emu.win.find_module(r"c:/game/RC.dll").map(|m| m.handle), Some(a), "slashes and case do not matter");
        assert!(emu.win.find_module(r"C:\Elsewhere\rc.dll").is_none(), "a different path is a different module");
    }

    #[test]
    fn module_file_name_reports_the_real_guest_path() {
        let mut emu = Emu::new();
        emu.mem.map(0x3000_0000, 0x1000);
        emu.win.virtual_files.insert("rc.dll".into(), reason_dll());
        let h = emu.load_library("rc.dll").unwrap();
        let (_, f) = crate::win32::kernel32::lookup("kernel32", "GetModuleFileNameA").unwrap();
        let mut a = [0u32; crate::win32::MAX_ARGS];
        (a[0], a[1], a[2]) = (h, 0x3000_0100, 260);
        f(&mut emu, &a).unwrap();
        assert_eq!(emu.mem.read_cstr(0x3000_0100).unwrap(), r"c:\game\rc.dll");
    }

    #[test]
    fn emulated_dll_handles_are_stable_and_distinct() {
        let mut emu = Emu::new();
        let k = emu.load_library("kernel32.dll").unwrap();
        let u = emu.load_library("USER32").unwrap();
        assert_ne!(k, u);
        assert_eq!(emu.load_library("KERNEL32").unwrap(), k);
        assert!(emu.win.module_by_handle(k).unwrap().exports.is_none());
    }
}

// ---- GUI: window class, message loop, WM_PAINT, shutdown ---------------------------------------

mod gui {
    use super::*;
    use crate::tests::util::Layout;
    use std::collections::HashMap;

    /// Tiny assembler with rel8 labels, enough to write a message loop by hand.
    #[derive(Default)]
    struct Asm {
        b: Vec<u8>,
        labels: HashMap<&'static str, usize>,
        fixups: Vec<(usize, &'static str)>,
    }

    impl Asm {
        fn emit(&mut self, bytes: &[u8]) -> &mut Self {
            self.b.extend_from_slice(bytes);
            self
        }
        fn label(&mut self, n: &'static str) -> &mut Self {
            self.labels.insert(n, self.b.len());
            self
        }
        fn jmp8(&mut self, op: u8, n: &'static str) -> &mut Self {
            self.b.extend_from_slice(&[op, 0]);
            self.fixups.push((self.b.len() - 1, n));
            self
        }
        fn push32(&mut self, v: u32) -> &mut Self {
            self.b.push(0x68);
            self.b.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn call(&mut self, slot: u32) -> &mut Self {
            self.b.extend_from_slice(&[0xFF, 0x15]);
            self.b.extend_from_slice(&slot.to_le_bytes());
            self
        }
        fn push_mem(&mut self, va: u32) -> &mut Self {
            self.b.extend_from_slice(&[0xFF, 0x35]);
            self.b.extend_from_slice(&va.to_le_bytes());
            self
        }
        fn store_eax(&mut self, va: u32) -> &mut Self {
            self.b.push(0xA3);
            self.b.extend_from_slice(&va.to_le_bytes());
            self
        }
        fn finish(mut self) -> (Vec<u8>, HashMap<&'static str, usize>) {
            for (pos, n) in std::mem::take(&mut self.fixups) {
                let rel = self.labels[n] as isize - (pos as isize + 1);
                self.b[pos] = rel as i8 as u8;
            }
            (self.b, self.labels)
        }
    }

    const WC: u32 = 0x100;
    const CLASS: u32 = 0x140;
    const TITLE: u32 = 0x150;
    const MSG: u32 = 0x160;
    const PS: u32 = 0x180;
    const RECT: u32 = 0x1C0;
    const HDC: u32 = 0x1D0;
    const HWND: u32 = 0x1D4;

    fn gui_program() -> Vec<u8> {
        let imports: &[(&str, &[&str])] = &[
            (
                "user32.dll",
                &["RegisterClassA", "CreateWindowExA", "GetMessageA", "DispatchMessageA", "PostQuitMessage",
                  "DefWindowProcA", "BeginPaint", "EndPaint", "FillRect"],
            ),
            ("gdi32.dll", &["CreateSolidBrush"]),
            ("kernel32.dll", &["ExitProcess"]),
        ];
        build_pe(imports, |l: &Layout| {
            let va = |o| l.code_va(o);
            let iat = |d, f| l.iat_va(d, f);
            let mut a = Asm::default();
            // --- entry ---
            a.push32(va(WC)).call(iat(0, 0)); // RegisterClassA(&wc)
            a.emit(&[0x6A, 0, 0x6A, 0, 0x6A, 0, 0x6A, 0]); // lpParam, hInst, hMenu, hParent
            a.push32(150).push32(200).emit(&[0x6A, 10, 0x6A, 10]); // h, w, y, x
            a.push32(0x10CF_0000).push32(va(TITLE)).push32(va(CLASS)).emit(&[0x6A, 0]);
            a.call(iat(0, 1)).store_eax(va(HWND)); // CreateWindowExA
            a.label("loop").emit(&[0x6A, 0, 0x6A, 0, 0x6A, 0]).push32(va(MSG)).call(iat(0, 2));
            a.emit(&[0x85, 0xC0]).jmp8(0x74, "done"); // test eax,eax; jz done
            a.push32(va(MSG)).call(iat(0, 3)).jmp8(0xEB, "loop"); // DispatchMessageA
            a.label("done").push_mem(va(MSG) + 8).call(iat(2, 0)); // ExitProcess(msg.wParam)
            // --- WndProc(hwnd, msg, wparam, lparam) ---
            a.label("wndproc").emit(&[0x8B, 0x44, 0x24, 0x08]); // mov eax,[esp+8]
            a.emit(&[0x83, 0xF8, 0x02]).jmp8(0x75, "chk_paint"); // WM_DESTROY?
            a.emit(&[0x6A, 0]).call(iat(0, 4)).emit(&[0x31, 0xC0, 0xC2, 16, 0]); // PostQuitMessage(0)
            a.label("chk_paint").emit(&[0x83, 0xF8, 0x0F]).jmp8(0x75, "other"); // WM_PAINT?
            a.push32(va(PS)).emit(&[0xFF, 0x74, 0x24, 0x08]).call(iat(0, 6)).store_eax(va(HDC));
            a.push32(0x0000_00FF).call(iat(1, 0)); // CreateSolidBrush(red)
            a.emit(&[0x50]).push32(va(RECT)).push_mem(va(HDC)).call(iat(0, 8)); // FillRect
            a.push32(va(PS)).emit(&[0xFF, 0x74, 0x24, 0x08]).call(iat(0, 7)); // EndPaint
            a.emit(&[0x31, 0xC0, 0xC2, 16, 0]);
            a.label("other").emit(&[0xFF, 0x74, 0x24, 0x10, 0xFF, 0x74, 0x24, 0x10, 0xFF, 0x74, 0x24, 0x10, 0xFF, 0x74, 0x24, 0x10]);
            a.call(iat(0, 5)).emit(&[0xC2, 16, 0]); // DefWindowProcA
            let (mut code, labels) = a.finish();

            assert!(code.len() <= WC as usize);
            code.resize(0x200, 0);
            let put = |c: &mut Vec<u8>, off: u32, v: u32| c[off as usize..off as usize + 4].copy_from_slice(&v.to_le_bytes());
            put(&mut code, WC + 4, va(labels["wndproc"] as u32)); // lpfnWndProc
            put(&mut code, WC + 28, 6); // hbrBackground = COLOR_WINDOW + 1
            put(&mut code, WC + 36, va(CLASS)); // lpszClassName
            code[CLASS as usize..CLASS as usize + 4].copy_from_slice(b"Win\0");
            code[TITLE as usize..TITLE as usize + 9].copy_from_slice(b"Hello XP\0");
            for (i, v) in [10u32, 10, 110, 60].iter().enumerate() {
                put(&mut code, RECT + 4 * i as u32, *v);
            }
            code
        })
    }

    #[test]
    fn window_paints_and_closes_cleanly() {
        let mut emu = Emu::new();
        let img = loader::load(&mut emu, &gui_program()).unwrap();
        assert!(img.unsupported.is_empty(), "{:?}", img.unsupported);

        // The app creates its window, paints, then blocks waiting for input.
        assert_eq!(emu.run(100_000), Stop::WaitForInput);
        let hwnd = emu.wm.windows[0].hwnd;
        assert_eq!(emu.wm.windows.len(), 1);
        assert_eq!(emu.wm.window(hwnd).unwrap().title, "Hello XP");
        assert!(emu.wm.window(hwnd).unwrap().visible);

        let desktop = emu.wm.compose();
        // Window at (10,10), 3px frame + 19px caption -> client origin (13, 32).
        assert_eq!(desktop.get_pixel(13 + 20, 32 + 20), Some(0xFF0000), "FillRect painted red");
        assert_eq!(desktop.get_pixel(13 + 150, 32 + 100), Some(0xFFFFFF), "class background is white");
        assert_eq!(desktop.get_pixel(60, 15), Some(0x0054E3), "caption bar");
        assert_eq!(desktop.get_pixel(500, 500), Some(0x3A6EA5), "desktop");

        // Re-running with no new input just waits again (the call is retried, not lost).
        assert_eq!(emu.run(1000), Stop::WaitForInput);

        // Host close button -> WM_CLOSE -> DefWindowProc -> DestroyWindow -> WM_DESTROY
        // -> PostQuitMessage -> GetMessage returns 0 -> ExitProcess(0).
        emu.wm.request_close();
        assert_eq!(emu.run(100_000), Stop::Exit(0));
        assert!(emu.wm.windows.is_empty());
    }

    #[test]
    fn mouse_input_reaches_the_wndproc_queue() {
        let mut emu = Emu::new();
        loader::load(&mut emu, &gui_program()).unwrap();
        assert_eq!(emu.run(100_000), Stop::WaitForInput);
        emu.wm.post_mouse(crate::gui::wm::WM_LBUTTONDOWN, 13 + 5, 32 + 6);
        // The click is dispatched to the (default) handling without disturbing the loop.
        assert_eq!(emu.run(100_000), Stop::WaitForInput);
        assert!(emu.wm.queue.is_empty());
    }
}

// ---- Real-world check: Windows XP Solitaire (only runs when the files are present) ----------

mod solitaire {
    use super::*;
    use crate::gui::wm::{WM_LBUTTONDOWN, WM_LBUTTONUP};

    fn boot() -> Option<Emu> {
        boot_seeded(None)
    }

    /// Boot the game; with a seed, its random shuffle is reproducible.
    fn boot_seeded(seed: Option<u32>) -> Option<Emu> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("exe/MS_Solitaire_Windows_XP");
        let exe = std::fs::read(dir.join("sol.exe")).ok()?;
        let mut emu = Emu::new();
        emu.win.fixed_time = seed;
        emu.win.dll_dirs.push(dir);
        loader::load(&mut emu, &exe).ok()?;
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput, "game should reach its message loop");
        Some(emu)
    }

    fn count(emu: &mut Emu, color: u32) -> usize {
        emu.wm.compose().pixels.iter().filter(|&&p| p == color).count()
    }

    #[test]
    fn boots_and_draws_a_table_of_cards() {
        let Some(mut emu) = boot() else { return };
        let top: Vec<_> = emu.wm.windows.iter().filter(|w| w.parent == 0).collect();
        assert_eq!(top.len(), 1, "one top-level window (plus child controls)");
        assert_eq!(top[0].title, "Solitaire");
        assert!(count(&mut emu, 0x008000) > 100_000, "green felt");
        assert!(count(&mut emu, 0xFFFFFF) > 10_000, "white card faces");
    }

    /// Click the Game menu, then the row at `row_y` (relative to the client origin).
    fn choose_game_menu_item(emu: &mut Emu, row_y: i32) {
        let hwnd = emu.wm.windows.iter().find(|w| w.parent == 0).unwrap().hwnd;
        let (ox, oy) = emu.wm.client_origin(hwnd).unwrap();
        emu.wm.post_mouse(WM_LBUTTONDOWN, ox + 14, oy - 9); // "Game" on the menu bar
        emu.wm.post_mouse(WM_LBUTTONUP, ox + 14, oy - 9);
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        emu.wm.post_mouse(WM_LBUTTONDOWN, ox + 22, oy + row_y);
        emu.wm.post_mouse(WM_LBUTTONUP, ox + 22, oy + row_y);
    }

    fn dialogs(emu: &Emu) -> Vec<u32> {
        emu.wm.windows.iter().filter(|w| w.is_dialog).map(|w| w.hwnd).collect()
    }

    fn save_shot(emu: &mut Emu, name: &str) {
        if let Some(dir) = std::env::var_os("XP_SHOT_DIR") {
            let region = emu.wm.content_bounds().unwrap();
            let shot = emu.wm.compose().crop(region.0, region.1, region.2, region.3);
            std::fs::write(std::path::Path::new(&dir).join(name), shot.to_ppm()).unwrap();
        }
    }

    /// Hash of what is drawn in the four foundation slots (top right of the table).
    fn foundation_fingerprint(emu: &mut Emu) -> u64 {
        use std::hash::{Hash, Hasher};
        let (cx, cy, _, _) = emu.wm.content_bounds().unwrap();
        let area = emu.wm.compose().crop(cx + 250, cy + 22, 370, 100);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        area.pixels.hash(&mut h);
        h.finish()
    }

    /// Press-release-press-release at one spot, 120 ms apart: a double click.
    fn double_click(emu: &mut Emu, x: i32, y: i32) {
        use crate::gui::wm::{WM_LBUTTONDOWN, WM_LBUTTONUP};
        let t = emu.wm.now_ms;
        emu.wm.post_mouse(WM_LBUTTONDOWN, x, y);
        emu.wm.post_mouse(WM_LBUTTONUP, x, y);
        emu.wm.now_ms = t + 120;
        emu.wm.post_mouse(WM_LBUTTONDOWN, x, y);
        emu.wm.post_mouse(WM_LBUTTONUP, x, y);
    }

    /// Classic (XP) Solitaire does not flip an exposed face-down card by itself; you click
    /// it. (Automatic flipping came with the later Vista/7 versions.)
    #[test]
    fn exposed_face_down_card_flips_only_when_clicked() {
        use crate::gui::wm::{WM_LBUTTONDOWN, WM_LBUTTONUP};
        // Seed 1 deals an ace face-up in the 4th column, with face-down cards under it.
        let Some(mut emu) = boot_seeded(Some(1)) else { return };
        let (cx, cy, _, _) = emu.wm.content_bounds().unwrap();
        let column = (cx + 47 + 82 * 3, cy + 125 + 6 * 3 + 40);
        let spot = |emu: &mut Emu| emu.wm.compose().crop(cx + 259, cy + 150, 60, 60).pixels;

        double_click(&mut emu, column.0, column.1);
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        let after_move = spot(&mut emu);

        emu.wm.now_ms += 3000; // time passing alone must not flip it
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert_eq!(spot(&mut emu), after_move, "still face down after waiting");

        let (x, y) = (cx + 47 + 82 * 3, cy + 125 + 6 * 2 + 30);
        emu.wm.post_mouse(WM_LBUTTONDOWN, x, y);
        emu.wm.post_mouse(WM_LBUTTONUP, x, y);
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert_ne!(spot(&mut emu), after_move, "one click turns it face up");
    }

    #[test]
    fn solitaire_asks_for_double_clicks() {
        let Some(emu) = boot() else { return };
        let class = &emu.wm.windows.iter().find(|w| w.parent == 0).unwrap().class;
        let style = emu.wm.classes[&class.to_ascii_lowercase()].style;
        assert!(style & crate::gui::wm::CS_DBLCLKS != 0, "class style {style:#x} lacks CS_DBLCLKS");
    }

    #[test]
    fn double_clicking_an_ace_sends_it_to_a_foundation_pile() {
        if boot().is_none() {
            return;
        }
        // Different seeds deal different games; find one where an ace is face-up on the table.
        let mut moved_in_seeds = Vec::new();
        for seed in 0..120u32 {
            let mut emu = boot_seeded(Some(seed)).unwrap();
            let (cx, cy, _, _) = emu.wm.content_bounds().unwrap();
            let before = foundation_fingerprint(&mut emu);
            // Face-up card of tableau column i: about (47 + 82i, 125 + 6i + 40) in the game's own coordinates.
            for col in 0..7 {
                let (x, y) = (cx + 47 + 82 * col, cy + 125 + 6 * col + 40);
                double_click(&mut emu, x, y);
                assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
                if foundation_fingerprint(&mut emu) != before {
                    moved_in_seeds.push((seed, col));
                    break;
                }
            }
            if moved_in_seeds.len() >= 3 {
                break;
            }
        }
        assert!(
            !moved_in_seeds.is_empty(),
            "no double click ever moved a card to a foundation pile"
        );
        eprintln!("double click moved a card in (seed, column): {moved_in_seeds:?}");

        // Control: replay the same deal and clicks with double clicks switched off. Nothing
        // may move, which shows it is the double click that does the work.
        let (seed, col) = moved_in_seeds[0];
        let mut emu = boot_seeded(Some(seed)).unwrap();
        for c in emu.wm.classes.values_mut() {
            c.style &= !crate::gui::wm::CS_DBLCLKS;
        }
        let (cx, cy, _, _) = emu.wm.content_bounds().unwrap();
        let before = foundation_fingerprint(&mut emu);
        double_click(&mut emu, cx + 47 + 82 * col, cy + 125 + 6 * col + 40);
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert_eq!(foundation_fingerprint(&mut emu), before, "two plain clicks must not move a card");
    }

    #[test]
    fn options_menu_item_opens_a_real_dialog_and_ok_closes_it() {
        let Some(mut emu) = boot() else { return };
        choose_game_menu_item(&mut emu, 70); // Deal, separator, Undo, Deck..., Options...
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput, "modal dialog waits for the user");
        let dlg = dialogs(&emu);
        assert_eq!(dlg.len(), 1, "the Options dialog exists");
        let d = emu.wm.window(dlg[0]).unwrap();
        assert!(d.visible && d.title.contains("Options"), "title was {:?}", d.title);
        assert!(emu.wm.windows.iter().filter(|w| w.parent == dlg[0]).count() >= 6, "radio buttons, check boxes, OK/Cancel");
        save_shot(&mut emu, "options.ppm");

        // Press Enter: IDOK -> EndDialog -> DialogBoxParam returns and the game carries on.
        emu.wm.post_key(0x0D, true);
        emu.wm.post_key(0x0D, false);
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput, "back in the game's own message loop");
        assert!(dialogs(&emu).is_empty(), "dialog closed");
        assert!(emu.wm.modal.is_none());
        assert!(emu.nested.is_empty(), "no leaked callback frames");

        emu.wm.request_close();
        assert!(matches!(emu.run(50_000_000), Stop::Exit(_)));
    }

    fn click_control(emu: &mut Emu, dlg: u32, label: &str) {
        let child = emu
            .wm
            .windows
            .iter()
            .find(|w| w.parent == dlg && crate::gui::wm::clean_label(&w.title) == label)
            .unwrap_or_else(|| panic!("no control labelled {label:?}"))
            .hwnd;
        let (ox, oy) = emu.wm.client_origin(child).unwrap();
        let (w, h) = emu.wm.client_size(child).unwrap();
        for msg in [WM_LBUTTONDOWN, WM_LBUTTONUP] {
            emu.wm.post_mouse(msg, ox + w / 2, oy + h / 2);
        }
    }

    fn checked(emu: &Emu, dlg: u32, label: &str) -> u32 {
        emu.wm.windows.iter().find(|w| w.parent == dlg && crate::gui::wm::clean_label(&w.title) == label).unwrap().check
    }

    #[test]
    fn options_dialog_controls_respond_to_clicks_and_cancel_closes() {
        let Some(mut emu) = boot() else { return };
        choose_game_menu_item(&mut emu, 70);
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        let dlg = dialogs(&emu)[0];
        assert_eq!((checked(&emu, dlg, "Draw One"), checked(&emu, dlg, "Draw Three")), (0, 1), "starts on Draw Three");

        click_control(&mut emu, dlg, "Draw One");
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert_eq!((checked(&emu, dlg, "Draw One"), checked(&emu, dlg, "Draw Three")), (1, 0), "radio selection moved");
        assert_eq!(checked(&emu, dlg, "Vegas"), 0, "other group untouched");

        let timed = checked(&emu, dlg, "Timed game");
        click_control(&mut emu, dlg, "Timed game");
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert_ne!(checked(&emu, dlg, "Timed game"), timed, "check box toggled");

        click_control(&mut emu, dlg, "Cancel");
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert!(dialogs(&emu).is_empty(), "Cancel closed the dialog");

        emu.wm.request_close();
        assert!(matches!(emu.run(50_000_000), Stop::Exit(_)));
    }

    #[test]
    fn closing_the_host_window_cancels_a_dialog_first() {
        let Some(mut emu) = boot() else { return };
        choose_game_menu_item(&mut emu, 70);
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert_eq!(dialogs(&emu).len(), 1);
        emu.wm.request_close(); // targets the topmost window: the dialog -> WM_CLOSE -> IDCANCEL
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput);
        assert!(dialogs(&emu).is_empty(), "the dialog closed; the game is still running");
        emu.wm.request_close();
        assert!(matches!(emu.run(50_000_000), Stop::Exit(_)), "second close quits the game");
    }

    #[test]
    fn clicking_the_stock_turns_a_card_and_the_game_keeps_running() {
        let Some(mut emu) = boot() else { return };
        let before = emu.wm.compose().pixels.clone();
        // Stock pile (top-left card back), then release.
        for msg in [WM_LBUTTONDOWN, WM_LBUTTONUP] {
            emu.wm.post_mouse(msg, 90, 75);
        }
        assert_eq!(emu.run(50_000_000), Stop::WaitForInput, "game handled the click and went idle");
        let after = emu.wm.compose().pixels.clone();
        let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        assert!(changed > 2_000, "the screen changed after the click ({changed} pixels)");

        // And it still shuts down cleanly.
        emu.wm.request_close();
        assert!(matches!(emu.run(50_000_000), Stop::Exit(_)), "closes without hanging or crashing");
    }
}

// ---- a wait deep inside a callback must be resumable -------------------------------------------

mod nested_wait {
    use super::*;
    use crate::tests::util::Layout;

    /// Entry: create a hidden window, SendMessage(0x400) to it, ExitProcess(result).
    /// WndProc(0x400) blocks in GetMessage (a wait inside a callback inside SendMessage),
    /// then returns 77.
    fn program() -> Vec<u8> {
        let imports: &[(&str, &[&str])] = &[
            ("user32.dll", &["RegisterClassA", "CreateWindowExA", "SendMessageA", "GetMessageA", "DefWindowProcA"]),
            ("kernel32.dll", &["ExitProcess"]),
        ];
        build_pe(imports, |l: &Layout| {
            let va = |o| l.code_va(o);
            let iat = |d, f| l.iat_va(d, f);
            let push32 = |c: &mut Vec<u8>, v: u32| {
                c.push(0x68);
                c.extend_from_slice(&v.to_le_bytes());
            };
            let call = |c: &mut Vec<u8>, slot: u32| {
                c.extend_from_slice(&[0xFF, 0x15]);
                c.extend_from_slice(&slot.to_le_bytes());
            };
            const WC: u32 = 0x100;
            const NAME: u32 = 0x140;
            const MSG: u32 = 0x160;
            let mut c = Vec::new();
            push32(&mut c, va(WC));
            call(&mut c, iat(0, 0)); // RegisterClassA
            c.extend_from_slice(&[0x6A, 0, 0x6A, 0, 0x6A, 0, 0x6A, 0, 0x6A, 50, 0x6A, 50, 0x6A, 0, 0x6A, 0, 0x6A, 0]);
            push32(&mut c, va(NAME)); // title
            push32(&mut c, va(NAME)); // class
            c.extend_from_slice(&[0x6A, 0]);
            call(&mut c, iat(0, 1)); // CreateWindowExA
            c.extend_from_slice(&[0x6A, 0, 0x6A, 0]); // lParam, wParam
            push32(&mut c, 0x400);
            c.push(0x50); // push eax (hwnd)
            call(&mut c, iat(0, 2)); // SendMessageA
            c.push(0x50);
            call(&mut c, iat(1, 0)); // ExitProcess(result)
            let wndproc = c.len() as u32;
            c.extend_from_slice(&[0x8B, 0x44, 0x24, 0x08, 0x3D, 0x00, 0x04, 0, 0]); // msg == 0x400 ?
            let jne_at = c.len();
            c.extend_from_slice(&[0x75, 0]);
            c.extend_from_slice(&[0x6A, 0, 0x6A, 0, 0x6A, 0]);
            push32(&mut c, va(MSG));
            call(&mut c, iat(0, 3)); // GetMessageA: blocks until input arrives
            c.extend_from_slice(&[0xB8, 77, 0, 0, 0, 0xC2, 16, 0]);
            let other = c.len();
            c[jne_at + 1] = (other - (jne_at + 2)) as u8;
            c.extend_from_slice(&[0xFF, 0x74, 0x24, 0x10].repeat(4));
            call(&mut c, iat(0, 4)); // DefWindowProcA
            c.extend_from_slice(&[0xC2, 16, 0]);
            assert!(c.len() <= WC as usize);
            c.resize(0x200, 0);
            let put = |c: &mut Vec<u8>, off: u32, v: u32| c[off as usize..off as usize + 4].copy_from_slice(&v.to_le_bytes());
            put(&mut c, WC + 4, va(wndproc));
            put(&mut c, WC + 36, va(NAME));
            c[NAME as usize..NAME as usize + 2].copy_from_slice(b"W\0");
            c
        })
    }

    #[test]
    fn a_wait_inside_a_callback_resumes_and_returns_through_the_caller() {
        let mut emu = Emu::new();
        loader::load(&mut emu, &program()).unwrap();
        assert_eq!(emu.run(100_000), Stop::WaitForInput, "blocked in GetMessage inside the WndProc");
        assert_eq!(emu.run(1_000), Stop::WaitForInput, "still waiting, nothing lost on a retry");
        emu.wm.post(0, 0x401, 0, 0);
        assert_eq!(
            emu.run(100_000),
            Stop::Exit(77),
            "the WndProc's result came back through SendMessage after the wait"
        );
        assert!(emu.nested.is_empty(), "no leftover callback frames");
    }
}
