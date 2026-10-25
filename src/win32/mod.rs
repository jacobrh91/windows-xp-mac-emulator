//! High-level emulation of the Win32 API. Imports resolve to "stub" addresses in an
//! unmapped region; when the CPU jumps there, a Rust handler runs instead and the
//! stdcall return is performed for the guest.

pub mod kernel32;
pub mod kernel32_extra;
pub mod msvcrt;
pub mod other_dlls;
pub mod seh;
pub mod threads;
pub mod vfs;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::cpu::{EAX, ESP, Emu, Res, Stop};

pub const HLE_BASE: u32 = 0x7000_0000;
const STUB_SIZE: u32 = 4;
const MAX_STUBS: u32 = 0x1_0000;
pub const HLE_END: u32 = HLE_BASE + MAX_STUBS * STUB_SIZE;

pub const HEAP_BASE: u32 = 0x3000_0000;
const HEAP_TAG_LIVE: u32 = 0x5041_4548; // "HEAP"
const HEAP_TAG_FREE: u32 = 0x4545_5246; // "FREE"
pub const VIRTUAL_BASE: u32 = 0x4000_0000;
pub const STACK_TOP: u32 = 0x0012_0000;
pub const STACK_SIZE: u32 = 0x0010_0000;
pub const TEB_ADDR: u32 = 0x7FFD_E000;
pub const PEB_ADDR: u32 = 0x7FFD_F000;

const PROCESS_HEAP: u32 = 0x1EAB_0000;
const MODULE_BASE: u32 = 0x7800_0000;
const MODULE_STRIDE: u32 = 0x1_0000;

pub const MAX_ARGS: usize = 12;
/// OR'd into a handler's argc to mark it cdecl: the caller, not the callee, pops arguments.
pub const CDECL: usize = 0x100;
pub type Args = [u32; MAX_ARGS];
pub type Handler = fn(&mut Emu, &Args) -> Res<u32>;

pub struct Stub {
    pub dll: String,
    pub name: String,
    pub argc: usize,
    pub handler: Option<Handler>,
}

/// A DLL the guest can see: either one we emulate in Rust (`exports == None`, fake
/// handle) or a real PE mapped into guest memory.
pub struct Module {
    /// Base name, lower-case, without the extension ("semiles").
    pub name: String,
    /// Full guest path, lower-case, for modules loaded from a file (empty for emulated DLLs).
    pub path: String,
    /// LoadLibrary count; the module is unloaded when FreeLibrary brings it to zero.
    pub refs: u32,
    /// DllMain address, or 0.
    pub entry: u32,
    pub handle: u32,
    /// Size of the mapped image (0 for emulated DLLs).
    pub size: u32,
    pub exports: Option<Exports>,
}

#[derive(Default)]
pub struct Exports {
    pub by_name: HashMap<String, u32>,
    pub by_ordinal: HashMap<u32, u32>,
}

/// DLLs we implement ourselves, so they are never looked up on disk.
const EMULATED_DLLS: &[&str] = &[
    "kernel32", "user32", "gdi32", "advapi32", "ntdll", "shell32", "ole32", "oleaut32", "comctl32",
    "comdlg32", "version", "winmm", "ws2_32", "wsock32", "msvcrt", "ddraw", "d3d8", "d3d9",
    "dsound", "dinput", "dinput8", "shlwapi", "imm32", "opengl32", "glu32", "setupapi", "wininet",
    "winspool", "msacm32", "psapi", "rpcrt4", "mpr", "netapi32", "iphlpapi",
];

pub struct Win32 {
    pub(crate) stubs: Vec<Stub>,
    stub_index: HashMap<(String, String), u32>,
    pub modules: Vec<Module>,
    /// In-memory "files" (lowercase name with extension -> bytes), checked before dll_dirs.
    pub virtual_files: HashMap<String, Vec<u8>>,
    pub dll_dirs: Vec<PathBuf>,
    pub next_dll_base: u32,
    pub image_base: u32,
    pub image_size: u32,
    heap_next: u32,
    heap_end: u32,
    /// Freed heap blocks by total size.
    heap_free: std::collections::BTreeMap<u32, Vec<u32>>,
    /// Sizes of `alloc_virtual` regions, so they can be released.
    virtual_sizes: HashMap<u32, u32>,
    virtual_next: u32,
    pub(crate) tls: Vec<u32>,
    /// Next TLS slot index to hand out (slots are shared; their values are per thread).
    pub(crate) tls_next: u32,
    /// When set, the emulator's clock reads this instead of real time (tests).
    pub virtual_now: Option<u64>,
    pub last_error: u32,
    /// Everything the guest wrote to stdout/stderr.
    pub stdout: Vec<u8>,
    /// MessageBox calls, as "title: text".
    pub messages: Vec<String>,
    pub(crate) cmdline: u32,
    pub(crate) data_syms: HashMap<String, u32>,
    pub rand_seed: u32,
    /// When set, the C runtime's time() returns this instead of the wall clock (tests).
    pub fixed_time: Option<u32>,
    /// Everything passed to OutputDebugString.
    pub debug_output: Vec<String>,
    /// The silent sound card and multimedia timers.
    pub audio: crate::media::audio::AudioState,
    pub d3d: crate::d3d::d3d8::D3d8,
    pub bink: crate::media::bink::BinkState,
    pub hooks: crate::hooks::HookState,
    pub mp3: crate::media::miles_mp3::Mp3State,
    /// The process-wide handler set by SetUnhandledExceptionFilter (0 = none).
    pub unhandled_filter: u32,
    /// The sandboxed file system and kernel-object handle table.
    pub vfs: crate::win32::vfs::Vfs,
    /// Windows path of the running program, for GetModuleFileName.
    pub exe_path: String,
    pub env: std::collections::BTreeMap<String, String>,
    /// Log every API call to stderr (set TRACE_API=1 in the CLI).
    pub trace: bool,
    started: Instant,
}

/// "C:\\Game\\SeMiles.dll" -> "semiles": the file name alone, lower-case, no ".dll".
pub fn normalize_dll(dll: &str) -> String {
    let d = dll
        .rsplit(['\\', '/', ':'])
        .next()
        .unwrap_or(dll)
        .to_ascii_lowercase();
    d.strip_suffix(".dll").unwrap_or(&d).to_string()
}

type Provider = fn(&str, &str) -> Option<(usize, Handler)>;

/// Every module that implements part of the API, asked in this order; the first that knows
/// the function wins. Each answers `None` for anything it does not implement.
const PROVIDERS: &[Provider] = &[
    core_api,
    kernel32_extra::lookup,
    crate::gui::user32::lookup,
    msvcrt::lookup,
    threads::lookup,
    crate::media::audio::lookup,
    crate::d3d::d3d8::lookup,
    crate::media::bink::lookup,
    crate::hooks::lookup,
    kernel32::lookup,
    other_dlls::lookup,
];

/// (argument bytes / 4, handler) for a supported API.
pub(crate) fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    PROVIDERS.iter().find_map(|p| p(dll, name))
}

/// Process basics: exit, heap, module handles, timing and the like.
fn core_api(dll: &str, name: &str) -> Option<(usize, Handler)> {
    let r: (usize, Handler) = match (dll, name) {
        ("__emu", "entry_return") => (0, |e, _| Err(Stop::Exit(e.regs[EAX]))),
        ("kernel32", "ExitProcess") => (1, |_, a| Err(Stop::Exit(a[0]))),
        ("kernel32", "GetModuleHandleA") => (1, k32_get_module_handle),
        ("kernel32", "LoadLibraryA") => (1, k32_load_library),
        ("kernel32", "GetProcAddress") => (2, k32_get_proc_address),
        ("kernel32", "GetCommandLineA") => (0, k32_get_command_line),
        ("kernel32", "GetStdHandle") => (1, |_, a| {
            // STD_INPUT/OUTPUT/ERROR_HANDLE are -10/-11/-12 -> fake handles 0x10/0x11/0x12.
            Ok(0x10u32.wrapping_add(a[0].wrapping_neg().wrapping_sub(10)))
        }),
        ("kernel32", "WriteFile") => (5, k32_write_file),
        ("kernel32", "GetLastError") => (0, |e, _| Ok(e.win.last_error)),
        ("kernel32", "SetLastError") => (1, |e, a| {
            e.win.last_error = a[0];
            Ok(0)
        }),
        ("kernel32", "GetProcessHeap") => (0, |_, _| Ok(PROCESS_HEAP)),
        ("kernel32", "HeapCreate") => (3, |_, _| Ok(PROCESS_HEAP)),
        ("kernel32", "HeapDestroy") => (1, |_, _| Ok(1)),
        ("kernel32", "HeapAlloc") => (3, |e, a| {
            let p = e.heap_alloc(a[2])?;
            if a[1] & 8 != 0 {
                e.mem.write_bytes(p, &vec![0; a[2] as usize])?; // HEAP_ZERO_MEMORY
            }
            Ok(p)
        }),
        ("kernel32", "HeapFree") => (3, |e, a| Ok(e.heap_free(a[2]) as u32)),
        ("kernel32", "HeapSize") => (3, |e, a| Ok(e.mem.read(a[2].wrapping_sub(16), 4)?)),
        ("kernel32", "HeapReAlloc") => (4, k32_heap_realloc),
        ("kernel32", "VirtualAlloc") => (4, k32_virtual_alloc),
        ("kernel32", "VirtualFree") => (3, |e, a| {
            if a[2] & 0x8000 != 0 {
                e.free_virtual(a[0]); // MEM_RELEASE; decommits keep the pages (harmless)
            }
            Ok(1)
        }),
        ("kernel32", "GetVersion") => (0, |_, _| Ok(0x0A28_0105)),
        ("kernel32", "GetVersionExA") => (1, k32_get_version_ex),
        ("kernel32", "GetTickCount") => (0, |e, _| Ok(e.win.now_ms() as u32)),
        ("kernel32", "QueryPerformanceCounter") => (1, |e, a| {
            let ns = e.win.started.elapsed().as_nanos() as u64;
            e.mem.write(a[0], 4, ns as u32)?;
            e.mem.write(a[0] + 4, 4, (ns >> 32) as u32)?;
            Ok(1)
        }),
        ("kernel32", "QueryPerformanceFrequency") => (1, |e, a| {
            e.mem.write(a[0], 4, 1_000_000_000)?;
            e.mem.write(a[0] + 4, 4, 0)?;
            Ok(1)
        }),
        ("kernel32", "GetCurrentProcess") => (0, |_, _| Ok(0xFFFF_FFFF)),
        ("kernel32", "GetCurrentProcessId") => (0, |_, _| Ok(0x400)),
        ("kernel32", "lstrlenA") => (1, |e, a| Ok(e.mem.read_cstr(a[0])?.len() as u32)),
        ("kernel32", "SetUnhandledExceptionFilter") => (1, |e, a| {
            Ok(std::mem::replace(&mut e.win.unhandled_filter, a[0]))
        }),
        ("kernel32", "GetACP") => (0, |_, _| Ok(1252)),
        ("kernel32", "GetStartupInfoA") => (1, |e, a| {
            e.mem.write_bytes(a[0], &[0; 68])?;
            e.mem.write(a[0], 4, 68)?;
            Ok(0)
        }),
        ("kernel32", "FreeLibrary") => (1, |e, a| Ok(e.free_library(a[0])? as u32)),
        ("user32", "MessageBoxA") => (4, |e, a| {
            let text = e.mem.read_cstr(a[1])?;
            let title = e.mem.read_cstr(a[2])?;
            e.win.messages.push(format!("{title}: {text}"));
            Ok(1)
        }),
        _ => return None,
    };
    Some(r)
}

impl Win32 {
    pub fn new() -> Self {
        Win32 {
            stubs: Vec::new(),
            stub_index: HashMap::new(),
            modules: Vec::new(),
            virtual_files: HashMap::new(),
            dll_dirs: Vec::new(),
            next_dll_base: 0x6000_0000,
            image_base: 0,
            image_size: 0,
            heap_next: HEAP_BASE,
            heap_end: VIRTUAL_BASE,
            heap_free: Default::default(),
            virtual_sizes: HashMap::new(),
            virtual_next: VIRTUAL_BASE,
            tls: Vec::new(),
            tls_next: 0,
            virtual_now: None,
            last_error: 0,
            stdout: Vec::new(),
            messages: Vec::new(),
            cmdline: 0,
            data_syms: HashMap::new(),
            rand_seed: 1,
            fixed_time: None,
            debug_output: Vec::new(),
            audio: Default::default(),
            d3d: Default::default(),
            bink: Default::default(),
            hooks: Default::default(),
            mp3: Default::default(),
            unhandled_filter: 0,
            vfs: crate::win32::vfs::Vfs::new(),
            exe_path: r"C:\Game\game.exe".to_string(),
            env: std::collections::BTreeMap::new(),
            trace: false,
            started: Instant::now(),
        }
    }

    /// Address the guest should call for `dll!name`. Unsupported functions still get a
    /// stub (so images load) but stop the emulator with `Unimplemented` when called.
    pub fn resolve(&mut self, dll: &str, name: &str) -> u32 {
        let dll = normalize_dll(dll);
        let key = (dll.clone(), name.to_string());
        if let Some(&addr) = self.stub_index.get(&key) {
            return addr;
        }
        let (argc, handler) = match lookup(&dll, name) {
            Some((argc, h)) => (argc, Some(h)),
            None => (0, None),
        };
        let addr = HLE_BASE + self.stubs.len() as u32 * STUB_SIZE;
        assert!(addr < HLE_END, "out of HLE stubs");
        self.stubs.push(Stub {
            dll,
            name: name.to_string(),
            argc,
            handler,
        });
        self.stub_index.insert(key, addr);
        addr
    }

    /// "module+0xoffset" for an address inside a loaded image, for crash reports.
    pub fn locate(&self, addr: u32) -> Option<String> {
        if self.image_size != 0
            && addr >= self.image_base
            && addr - self.image_base < self.image_size
        {
            return Some(format!("exe+{:#x}", addr - self.image_base));
        }
        self.modules
            .iter()
            .find(|m| m.size != 0 && addr >= m.handle && addr - m.handle < m.size)
            .map(|m| format!("{}+{:#x}", m.name, addr - m.handle))
    }

    /// The emulator's clock in milliseconds: real time since start, or the test override.
    pub fn now_ms(&self) -> u64 {
        self.virtual_now
            .unwrap_or_else(|| self.started.elapsed().as_millis() as u64)
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.now_ms()
    }

    pub fn is_supported(dll: &str, name: &str) -> bool {
        lookup(&normalize_dll(dll), name).is_some() || crate::win32::msvcrt::is_data_symbol(name)
    }

    pub fn is_emulated_dll(name: &str) -> bool {
        let n = normalize_dll(name);
        EMULATED_DLLS.contains(&n.as_str()) || (n == "binkw32" && crate::media::bink::enabled())
    }

    /// Index of the loaded module `name` refers to. A name with a path matches that exact
    /// file; a bare name matches any loaded module with that file name (as on Windows).
    pub fn module_index(&self, name: &str) -> Option<usize> {
        if name.contains(['\\', '/', ':']) {
            let mut full = self.vfs.normalize(name).to_ascii_lowercase();
            if !full.rsplit('\\').next().unwrap_or("").contains('.') {
                full.push_str(".dll");
            }
            if let Some(i) = self
                .modules
                .iter()
                .position(|m| !m.path.is_empty() && m.path == full)
            {
                return Some(i);
            }
            // A path to a DLL we emulate (C:\\WINDOWS\\system32\\kernel32.dll) is that emulated DLL.
            let base = normalize_dll(name);
            return self
                .modules
                .iter()
                .position(|m| m.path.is_empty() && m.name == base);
        }
        let base = normalize_dll(name);
        self.modules.iter().position(|m| m.name == base)
    }

    pub fn find_module(&self, name: &str) -> Option<&Module> {
        self.module_index(name).map(|i| &self.modules[i])
    }

    pub fn module_by_handle(&self, handle: u32) -> Option<&Module> {
        self.modules.iter().find(|m| m.handle == handle)
    }

    /// Fake handle for a DLL we emulate in Rust (stable per name).
    pub(crate) fn emulated_handle(&mut self, name: &str) -> u32 {
        if let Some(m) = self.find_module(name) {
            return m.handle;
        }
        let handle = MODULE_BASE + self.modules.len() as u32 * MODULE_STRIDE;
        self.modules.push(Module {
            name: normalize_dll(name),
            path: String::new(),
            refs: 1,
            entry: 0,
            handle,
            size: 0,
            exports: None,
        });
        handle
    }

    /// Bytes of a DLL file the guest can load, from virtual files or the search dirs.
    /// The bytes of a DLL the guest can load, and the guest path it should be known by.
    pub fn find_dll_file(&self, name: &str) -> Option<(Vec<u8>, String)> {
        let mut file = name.to_string();
        let last = file.rsplit(['\\', '/']).next().unwrap_or(&file);
        if !last.contains('.') {
            file.push_str(".dll");
        }
        let exe_dir = self
            .exe_path
            .rsplit_once('\\')
            .map_or(r"C:\", |(d, _)| d)
            .to_ascii_lowercase();
        // A path (relative to the current directory, or absolute) is resolved through the
        // sandboxed file system the game sees.
        if file.contains(['\\', '/', ':'])
            && let Ok(host) = self.vfs.resolve(&file)
            && host.is_file()
        {
            return std::fs::read(host)
                .ok()
                .map(|b| (b, self.vfs.normalize(&file).to_ascii_lowercase()));
        }
        let lower = file
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&file)
            .to_ascii_lowercase();
        let guest_path = format!("{exe_dir}\\{lower}");
        if let Some(b) = self.virtual_files.get(&lower) {
            return Some((b.clone(), guest_path));
        }
        for dir in &self.dll_dirs {
            for entry in std::fs::read_dir(dir).ok()?.flatten() {
                if entry.file_name().to_string_lossy().to_ascii_lowercase() == lower {
                    return std::fs::read(entry.path()).ok().map(|b| (b, guest_path));
                }
            }
        }
        // Finally, a bare name in the program's current directory.
        if let Ok(host) = self.vfs.resolve(&lower)
            && host.is_file()
        {
            return std::fs::read(host)
                .ok()
                .map(|b| (b, self.vfs.normalize(&lower).to_ascii_lowercase()));
        }
        None
    }
}

impl Emu {
    /// Run the Rust handler for the stub at `eip`, then perform the stdcall return.
    pub(crate) fn call_hle(&mut self) -> Res<()> {
        let idx = ((self.eip - HLE_BASE) / STUB_SIZE) as usize;
        let (raw_argc, handler, dll, name) = match self.win.stubs.get(idx) {
            Some(s) => (s.argc, s.handler, s.dll.clone(), s.name.clone()),
            None => {
                return Err(Stop::Unimplemented(format!(
                    "unknown stub at {:#x}",
                    self.eip
                )));
            }
        };
        let Some(handler) = handler else {
            return Err(Stop::Unimplemented(format!("{dll}!{name}")));
        };
        let (argc, cdecl) = (raw_argc & 0xff, raw_argc & CDECL != 0);
        let esp = self.regs[ESP];
        let mut args = [0u32; MAX_ARGS];
        for (i, slot) in args.iter_mut().enumerate().take(argc.min(MAX_ARGS)) {
            *slot = self.mem.read(esp.wrapping_add(4 + 4 * i as u32), 4)?;
        }
        let ret = handler(self, &args);
        if let Err(Stop::Fault(f)) = &ret {
            let ret_addr = self.mem.read(esp, 4).unwrap_or(0);
            let caller = self
                .win
                .locate(ret_addr)
                .unwrap_or_else(|| format!("{ret_addr:#x}"));
            let shown: Vec<String> = args.iter().take(argc).map(|a| format!("{a:#x}")).collect();
            eprintln!(
                "[api fault] {dll}!{name}({}) touched unmapped {:#x} <{caller}> thread {}",
                shown.join(", "),
                f.0,
                self.cur_tid()
            );
            let mut hist: Vec<String> = (1..=16u64)
                .map(|i| self.recent[((self.steps + 16 - i) & 15) as usize])
                .map(|a| self.win.locate(a).unwrap_or_else(|| format!("{a:#x}")))
                .collect();
            hist.reverse();
            eprintln!("[api fault]   recent: {}", hist.join(" "));
            for k in 0..64u32 {
                if let Some(loc) = self
                    .mem
                    .read(esp.wrapping_add(4 * k), 4)
                    .ok()
                    .and_then(|v| self.win.locate(v))
                {
                    eprintln!("[api fault]   stack+{:#x}: {loc}", 4 * k);
                }
            }
        }
        if self.win.trace {
            let shown: Vec<String> = args.iter().take(argc).map(|a| format!("{a:#x}")).collect();
            let result = match &ret {
                Ok(v) => format!("{v:#x}"),
                Err(s) => format!("{s:?}"),
            };
            let caller = self
                .mem
                .read(esp, 4)
                .ok()
                .and_then(|r| self.win.locate(r))
                .unwrap_or_default();
            eprintln!(
                "[api] {dll}!{name}({}) -> {result} <{caller}>",
                shown.join(", ")
            );
        }
        // A bad pointer passed to an API is an access violation in the program, as on Windows
        // (where it would fault inside the system DLL): its exception handlers get a say.
        if let Err(Stop::Fault(f)) = ret {
            let snapshot = (self.regs, self.eflags);
            return self.raise_hardware(Stop::Fault(f), self.eip, snapshot);
        }
        let ret = ret?;
        if std::mem::take(&mut self.control_transferred) {
            return Ok(()); // the handler already moved eip/esp (RaiseException)
        }
        self.regs[EAX] = ret;
        self.eip = self.pop()?;
        if !cdecl {
            self.regs[ESP] = self.regs[ESP].wrapping_add(4 * argc as u32);
        }
        Ok(())
    }

    /// Pick an address range of `size` bytes that nothing (such as a DLL loaded at its
    /// preferred base) has mapped yet.
    pub fn reserve(&mut self, size: u32) -> u32 {
        let span = (size.max(1) + 0xFFFF) & !0xFFFF;
        let mut base = self.win.virtual_next;
        let mut wrapped = false;
        loop {
            if base.checked_add(span).is_none_or(|end| end > HLE_BASE) {
                if wrapped {
                    break; // the address space is full; the caller will fault, visibly
                }
                // Released regions below are free again: search from the start.
                wrapped = true;
                base = VIRTUAL_BASE;
            }
            match self.mem.first_mapped(base, span) {
                Some(hit) => base = (hit + 0x1_0000) & !0xFFFF,
                None => break,
            }
        }
        self.win.virtual_next = base + span;
        base
    }

    /// Reserve and commit `size` bytes of fresh guest address space (64 KiB aligned).
    pub fn alloc_virtual(&mut self, size: u32) -> u32 {
        let base = self.reserve(size);
        self.mem.map(base, size);
        self.win.virtual_sizes.insert(base, size);
        base
    }

    /// Release a region from `alloc_virtual` (or VirtualAlloc). Unknown addresses are ignored.
    pub fn free_virtual(&mut self, base: u32) -> bool {
        match self.win.virtual_sizes.remove(&base) {
            Some(size) => {
                self.mem.unmap(base, size);
                true
            }
            None => false,
        }
    }

    /// Allocate guest heap memory. A 16-byte header precedes each block: the requested size,
    /// the block's total size and a tag that marks it as live. Freed blocks are reused,
    /// smallest fitting block first (larger ones are split).
    pub fn heap_alloc(&mut self, size: u32) -> Res<u32> {
        let total = (size.saturating_add(16 + 15)) & !15;
        let block = match self.win.heap_free.range(total..).next().map(|(&t, _)| t) {
            Some(found) => {
                let list = self.win.heap_free.get_mut(&found).unwrap();
                let block = list.pop().unwrap();
                if list.is_empty() {
                    self.win.heap_free.remove(&found);
                }
                if found - total >= 64 {
                    // Split: the rest goes back as its own free block.
                    let rest = block + total;
                    self.mem.write(rest + 4, 4, found - total)?;
                    self.mem.write(rest + 8, 4, HEAP_TAG_FREE)?;
                    self.win
                        .heap_free
                        .entry(found - total)
                        .or_default()
                        .push(rest);
                    self.mem.write(block + 4, 4, total)?;
                } else {
                    self.mem.write(block + 4, 4, found)?;
                }
                block
            }
            None => {
                if self
                    .win
                    .heap_next
                    .checked_add(total)
                    .is_none_or(|end| end > self.win.heap_end)
                {
                    // This arena is full: carry on in fresh address space.
                    let chunk = total.max(16 << 20);
                    self.win.heap_next = self.reserve(chunk);
                    self.win.heap_end = self.win.heap_next + chunk;
                }
                let block = self.win.heap_next;
                self.win.heap_next += total;
                self.mem.map(block, total);
                self.mem.write(block + 4, 4, total)?;
                block
            }
        };
        self.mem.write(block, 4, size)?;
        self.mem.write(block + 8, 4, HEAP_TAG_LIVE)?;
        Ok(block + 16)
    }

    /// Return a block from `heap_alloc`. Anything else (null, a double free, a pointer that
    /// was never allocated) is refused, as HeapFree does.
    pub fn heap_free(&mut self, p: u32) -> bool {
        if p < 16 || self.mem.read(p - 8, 4) != Ok(HEAP_TAG_LIVE) {
            return false;
        }
        let block = p - 16;
        let Ok(total) = self.mem.read(block + 4, 4) else {
            return false;
        };
        let _ = self.mem.write(block + 8, 4, HEAP_TAG_FREE);
        self.win.heap_free.entry(total).or_default().push(block);
        true
    }

    /// Usable size of a heap block (its capacity, not just what was asked for).
    pub fn heap_capacity(&self, p: u32) -> Option<u32> {
        (self.mem.read(p.wrapping_sub(8), 4).ok()? == HEAP_TAG_LIVE)
            .then(|| self.mem.read(p - 12, 4).ok().map(|t| t - 16))?
    }
}

fn k32_get_module_handle(e: &mut Emu, a: &Args) -> Res<u32> {
    if a[0] == 0 {
        return Ok(e.win.image_base);
    }
    let name = e.mem.read_cstr(a[0])?;
    if let Some(m) = e.win.find_module(&name) {
        return Ok(m.handle);
    }
    Ok(if Win32::is_emulated_dll(&name) {
        e.win.emulated_handle(&name)
    } else {
        0
    })
}

fn k32_load_library(e: &mut Emu, a: &Args) -> Res<u32> {
    let name = e.mem.read_cstr(a[0])?;
    let handle = e.load_library(&name)?;
    if handle == 0 {
        e.win.last_error = 126; // ERROR_MOD_NOT_FOUND
    }
    Ok(handle)
}

fn k32_get_proc_address(e: &mut Emu, a: &Args) -> Res<u32> {
    let (handle, name_ptr) = (a[0], a[1]);
    let Some(module) = e.win.module_by_handle(handle) else {
        return Ok(0);
    };
    if let Some(exports) = &module.exports {
        return Ok(if name_ptr < 0x1_0000 {
            exports.by_ordinal.get(&name_ptr).copied()
        } else {
            let name = e.mem.read_cstr(name_ptr)?;
            exports.by_name.get(&name).copied()
        }
        .unwrap_or(0));
    }
    if name_ptr < 0x1_0000 {
        return Ok(0); // ordinal import from an emulated DLL: unsupported
    }
    let name = e.mem.read_cstr(name_ptr)?;
    let dll = module.name.clone();
    if e.win.trace {
        eprintln!("[api]   GetProcAddress {dll}!{name}");
    }
    if !Win32::is_supported(&dll, &name) {
        return Ok(0);
    }
    Ok(e.win.resolve(&dll, &name))
}

fn k32_get_command_line(e: &mut Emu, _: &Args) -> Res<u32> {
    if e.win.cmdline == 0 {
        let s = b"game.exe\0";
        let p = e.heap_alloc(s.len() as u32)?;
        e.mem.write_bytes(p, s)?;
        e.win.cmdline = p;
    }
    Ok(e.win.cmdline)
}

fn k32_write_file(e: &mut Emu, a: &Args) -> Res<u32> {
    let (handle, buf, len, written) = (a[0], a[1], a[2], a[3]);
    if handle != 0x11 && handle != 0x12 {
        // A file opened through the sandboxed file system.
        let data = e.mem.read_bytes(buf, len)?;
        return match e.win.vfs.write(handle, &data) {
            Ok(n) => {
                if written != 0 {
                    e.mem.write(written, 4, n as u32)?;
                }
                Ok(1)
            }
            Err(code) => {
                e.win.last_error = code;
                Ok(0)
            }
        };
    }
    let data = e.mem.read_bytes(buf, len)?;
    e.win.stdout.extend_from_slice(&data);
    if written != 0 {
        e.mem.write(written, 4, len)?;
    }
    Ok(1)
}

fn k32_heap_realloc(e: &mut Emu, a: &Args) -> Res<u32> {
    let (flags, old, size) = (a[1], a[2], a[3]);
    const HEAP_ZERO_MEMORY: u32 = 8;
    const HEAP_REALLOC_IN_PLACE_ONLY: u32 = 0x10;
    let old_size = if old != 0 {
        e.mem.read(old - 16, 4)?
    } else {
        0
    };
    // Fits where it is?
    if old != 0 && e.heap_capacity(old).is_some_and(|cap| size <= cap) {
        if flags & HEAP_ZERO_MEMORY != 0 && size > old_size {
            e.mem
                .write_bytes(old + old_size, &vec![0; (size - old_size) as usize])?;
        }
        e.mem.write(old - 16, 4, size)?;
        return Ok(old);
    }
    if flags & HEAP_REALLOC_IN_PLACE_ONLY != 0 {
        return Ok(0);
    }
    let new = e.heap_alloc(size)?;
    if old != 0 {
        let data = e.mem.read_bytes(old, old_size.min(size))?;
        e.mem.write_bytes(new, &data)?;
        e.heap_free(old);
    }
    if flags & HEAP_ZERO_MEMORY != 0 && size > old_size {
        e.mem
            .write_bytes(new + old_size, &vec![0; (size - old_size) as usize])?;
    }
    Ok(new)
}

fn k32_virtual_alloc(e: &mut Emu, a: &Args) -> Res<u32> {
    let (addr, size) = (a[0], a[1]);
    let base = if addr != 0 {
        addr
    } else {
        let b = e.reserve(size);
        e.win.virtual_sizes.insert(b, size);
        b
    };
    e.mem.map(base, size);
    Ok(base)
}

fn k32_get_version_ex(e: &mut Emu, a: &Args) -> Res<u32> {
    let p = a[0];
    // OSVERSIONINFOA: size, major, minor, build, platform, szCSDVersion[128]
    e.mem.write(p + 4, 4, 5)?;
    e.mem.write(p + 8, 4, 1)?;
    e.mem.write(p + 12, 4, 2600)?;
    e.mem.write(p + 16, 4, 2)?;
    e.mem.write_bytes(p + 20, b"Service Pack 2\0")?;
    Ok(1)
}

#[cfg(test)]
mod heap_tests {
    use crate::cpu::Emu;

    #[test]
    fn freed_blocks_are_reused_and_split() {
        let mut e = Emu::new();
        let a = e.heap_alloc(100).unwrap();
        let b = e.heap_alloc(100).unwrap();
        assert_ne!(a, b);
        assert!(e.heap_free(a));
        assert_eq!(
            e.heap_alloc(90).unwrap(),
            a,
            "a freed block of the right size comes back"
        );
        assert!(e.heap_free(b));
        let small = e.heap_alloc(10).unwrap();
        assert_eq!(small, b, "a larger free block is used...");
        let rest = e.heap_alloc(10).unwrap();
        assert!(
            rest > b && rest < b + 128,
            "...and split, so its remainder is used next"
        );
    }

    #[test]
    fn bad_frees_are_refused() {
        let mut e = Emu::new();
        let a = e.heap_alloc(32).unwrap();
        assert!(e.heap_free(a));
        assert!(!e.heap_free(a), "double free");
        assert!(!e.heap_free(0), "null");
        assert!(!e.heap_free(a + 4), "not the start of a block");
    }

    #[test]
    fn a_steady_alloc_free_cycle_does_not_grow_the_heap() {
        let mut e = Emu::new();
        let first = e.heap_alloc(1000).unwrap();
        e.heap_free(first);
        for _ in 0..100_000 {
            let p = e.heap_alloc(1000).unwrap();
            assert_eq!(p, first);
            e.heap_free(p);
        }
    }

    #[test]
    fn released_virtual_memory_is_unmapped() {
        let mut e = Emu::new();
        let v = e.alloc_virtual(0x20000);
        assert!(e.mem.is_mapped(v));
        assert!(e.free_virtual(v));
        assert!(!e.mem.is_mapped(v));
        assert!(!e.free_virtual(v), "only once");
    }
}
