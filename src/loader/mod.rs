//! Maps PE32 images (the program and any DLLs it needs) into guest memory, applies
//! relocations, binds imports and prepares a Windows-style process (stack, TEB/PEB).

pub mod pe;

use std::collections::HashMap;
use std::fmt;

use crate::cpu::{ESP, Emu, Res, Stop};
use crate::loader::pe::{Pe, PeError};
use crate::memory::Fault;
use crate::win32::{Exports, Module, PEB_ADDR, STACK_SIZE, STACK_TOP, TEB_ADDR};

#[derive(Debug)]
pub enum LoadError {
    Pe(PeError),
    Fault(Fault),
    /// Running a DLL's entry point (DllMain) failed.
    Stop(Stop),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Pe(e) => write!(f, "{e}"),
            LoadError::Fault(Fault(a)) => write!(f, "image does not fit in guest memory ({a:#x})"),
            LoadError::Stop(s) => write!(f, "DllMain failed: {s:?}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<PeError> for LoadError {
    fn from(e: PeError) -> Self {
        LoadError::Pe(e)
    }
}

impl From<Fault> for LoadError {
    fn from(f: Fault) -> Self {
        LoadError::Fault(f)
    }
}

pub struct Image {
    pub base: u32,
    pub entry: u32,
    /// Imports the emulator has no implementation for, as "dll!name".
    pub unsupported: Vec<String>,
}

pub fn load(emu: &mut Emu, data: &[u8]) -> Result<Image, LoadError> {
    let pe = Pe::parse(data)?;
    let base = map_image(emu, &pe, data)?;
    emu.win.image_base = base;
    emu.win.image_size = pe.size_of_image;
    // The stack must exist before imports are bound: loading a DLL runs its DllMain.
    init_process(emu)?;
    let mut unsupported = Vec::new();
    bind_imports(emu, &pe, base, &mut unsupported)?;
    start_at(emu, base + pe.entry_rva)?;
    Ok(Image {
        base,
        entry: base + pe.entry_rva,
        unsupported,
    })
}

/// Load a DLL from raw bytes, run its DllMain(DLL_PROCESS_ATTACH) and return its handle.
pub fn load_dll(emu: &mut Emu, data: &[u8], name: &str, path: &str) -> Result<u32, LoadError> {
    let pe = Pe::parse(data)?;
    let base = map_image(emu, &pe, data)?;

    let mut exports = Exports::default();
    for ex in &pe.exports {
        if let Some(n) = &ex.name {
            exports.by_name.insert(n.clone(), base + ex.rva);
        }
        exports.by_ordinal.insert(ex.ordinal, base + ex.rva);
    }
    // Register before binding imports/DllMain so circular imports find us.
    let entry = if pe.is_dll && pe.entry_rva != 0 {
        base + pe.entry_rva
    } else {
        0
    };
    emu.win.modules.push(Module {
        name: crate::win32::normalize_dll(name),
        path: path.to_string(),
        refs: 1,
        entry,
        handle: base,
        size: pe.size_of_image,
        exports: Some(exports),
    });

    bind_imports(emu, &pe, base, &mut Vec::new())?;
    crate::hooks::apply(emu, &crate::win32::normalize_dll(name), base);
    if pe.is_dll && pe.entry_rva != 0 {
        const DLL_PROCESS_ATTACH: u32 = 1;
        emu.call_function(base + pe.entry_rva, &[base, DLL_PROCESS_ATTACH, 0])
            .map_err(LoadError::Stop)?;
    }
    Ok(base)
}

impl Emu {
    /// LoadLibrary: returns a module handle, or 0 if the DLL is unknown. Loading a module that
    /// is already loaded just counts another reference.
    pub fn load_library(&mut self, name: &str) -> Res<u32> {
        if let Some(i) = self.win.module_index(name) {
            self.win.modules[i].refs += 1;
            return Ok(self.win.modules[i].handle);
        }
        if crate::win32::Win32::is_emulated_dll(name) {
            return Ok(self.win.emulated_handle(name));
        }
        match self.win.find_dll_file(name) {
            Some((bytes, path)) => load_dll(self, &bytes, name, &path)
                .map_err(|e| Stop::Unimplemented(format!("loading {name}: {e}"))),
            None => Ok(0),
        }
    }

    /// FreeLibrary: drop a reference; at zero the DLL gets DLL_PROCESS_DETACH and its memory is
    /// released, so loading it again starts it from scratch (its static objects included).
    pub fn free_library(&mut self, handle: u32) -> Res<bool> {
        let Some(i) = self.win.modules.iter().position(|m| m.handle == handle) else {
            return Ok(false);
        };
        let m = &mut self.win.modules[i];
        if m.exports.is_none() {
            return Ok(true); // an emulated DLL has nothing to unload
        }
        m.refs = m.refs.saturating_sub(1);
        if m.refs > 0 {
            return Ok(true);
        }
        let (entry, size) = (m.entry, m.size);
        if entry != 0 {
            const DLL_PROCESS_DETACH: u32 = 0;
            // The DLL's destructors run now. A fault in them must not take the program down.
            match self.call_function(entry, &[handle, DLL_PROCESS_DETACH, 0]) {
                Ok(_) | Err(Stop::Fault(_)) => {}
                Err(other) => return Err(other),
            }
        }
        if let Some(i) = self.win.modules.iter().position(|m| m.handle == handle) {
            self.win.modules.remove(i);
        }
        self.mem.unmap(handle, size);
        Ok(true)
    }
}

/// Pick where to put an image: its preferred base if free, else the next free slot.
fn pick_base(emu: &mut Emu, pe: &Pe) -> u32 {
    let free = (0..pe.size_of_image.div_ceil(0x1000))
        .all(|p| !emu.mem.is_mapped(pe.image_base.wrapping_add(p * 0x1000)));
    if free && pe.image_base != 0 {
        return pe.image_base;
    }
    let base = emu.win.next_dll_base;
    emu.win.next_dll_base += (pe.size_of_image + 0xFFFF) & !0xFFFF;
    base
}

/// Copy headers and sections to guest memory and apply base relocations.
fn map_image(emu: &mut Emu, pe: &Pe, data: &[u8]) -> Result<u32, LoadError> {
    let base = pick_base(emu, pe);
    emu.mem.map(base, pe.size_of_image);

    let headers = data
        .get(..pe.size_of_headers as usize)
        .ok_or(PeError::Truncated("headers"))?;
    emu.mem.write_bytes(base, headers)?;

    for s in &pe.sections {
        let start = s.raw_offset as usize;
        let bytes = data
            .get(start..start.saturating_add(s.raw_size as usize))
            .ok_or(PeError::Truncated("section data"))?;
        emu.mem.write_bytes(base + s.virtual_address, bytes)?;
    }

    let delta = base.wrapping_sub(pe.image_base);
    if delta != 0 {
        // Without a relocation table the image cannot be fixed up. That only matters for code;
        // resource-only DLLs (strings, dialogs) work wherever they land, so load them anyway.
        for &rva in &pe.relocs {
            let addr = base + rva;
            let v = emu.mem.read(addr, 4)?;
            emu.mem.write(addr, 4, v.wrapping_add(delta))?;
        }
    }
    Ok(base)
}

/// Fill each import address table slot with either a real DLL's export or an HLE stub.
fn bind_imports(
    emu: &mut Emu,
    pe: &Pe,
    base: u32,
    unsupported: &mut Vec<String>,
) -> Result<(), LoadError> {
    let mut real_exports: HashMap<String, Option<(HashMap<String, u32>, HashMap<u32, u32>)>> =
        HashMap::new();
    for imp in &pe.imports {
        let key = imp.dll.to_ascii_lowercase();
        if !real_exports.contains_key(&key) {
            let handle = emu.load_library(&imp.dll).map_err(LoadError::Stop)?;
            let ex = emu
                .win
                .module_by_handle(handle)
                .and_then(|m| m.exports.as_ref())
                .map(|e| (e.by_name.clone(), e.by_ordinal.clone()));
            real_exports.insert(key.clone(), ex);
        }
        let real = &real_exports[&key];
        for (i, name) in imp.functions.iter().enumerate() {
            let from_real =
                real.as_ref()
                    .and_then(|(by_name, by_ord)| match name.strip_prefix('#') {
                        Some(n) => n.parse().ok().and_then(|o| by_ord.get(&o)),
                        None => by_name.get(name),
                    });
            let data =
                crate::win32::msvcrt::data_symbol(emu, &imp.dll, name).map_err(LoadError::Stop)?;
            let addr = match (from_real, data) {
                (Some(&a), _) => a,
                (None, Some(a)) => a,
                (None, None) => {
                    if !crate::win32::Win32::is_supported(&imp.dll, name) {
                        unsupported.push(format!("{}!{}", imp.dll, name));
                    }
                    emu.win.resolve(&imp.dll, name)
                }
            };
            emu.mem.write(base + imp.iat_rva + 4 * i as u32, 4, addr)?;
        }
    }
    Ok(())
}

/// Stack, TEB and PEB. Safe to call more than once.
pub fn init_process(emu: &mut Emu) -> Result<(), Fault> {
    emu.mem.map(STACK_TOP - STACK_SIZE, STACK_SIZE);
    emu.mem.map(TEB_ADDR, 0x2000);

    // TEB: exception list, stack base/limit, self pointer, PEB pointer.
    emu.mem.write(TEB_ADDR, 4, 0xFFFF_FFFF)?;
    emu.mem.write(TEB_ADDR + 4, 4, STACK_TOP)?;
    emu.mem.write(TEB_ADDR + 8, 4, STACK_TOP - STACK_SIZE)?;
    emu.mem.write(TEB_ADDR + 0x18, 4, TEB_ADDR)?;
    emu.mem.write(TEB_ADDR + 0x30, 4, PEB_ADDR)?;
    // PEB: ImageBaseAddress, OSMajor/Minor/Build (XP SP2 = 5.1.2600).
    emu.mem.write(PEB_ADDR + 8, 4, emu.win.image_base)?;
    emu.mem.write(PEB_ADDR + 0xA4, 4, 5)?;
    emu.mem.write(PEB_ADDR + 0xA8, 4, 1)?;
    emu.mem.write(PEB_ADDR + 0xAC, 2, 2600)?;
    emu.fs_base = TEB_ADDR;
    if emu.regs[ESP] == 0 {
        emu.regs[ESP] = STACK_TOP - 16;
    }
    Ok(())
}

/// Point the CPU at the entry point, with its return address wired to ExitProcess(eax).
fn start_at(emu: &mut Emu, entry: u32) -> Result<(), Fault> {
    let exit_stub = emu.win.resolve("__emu", "entry_return");
    emu.regs[ESP] = STACK_TOP - 16;
    emu.push(exit_stub).map_err(|_| Fault(STACK_TOP))?;
    emu.eip = entry;
    Ok(())
}
