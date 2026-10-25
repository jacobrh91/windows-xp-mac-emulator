//! Minimal PE32 (32-bit Windows executable) parser.
//! Parses headers, sections and the import table. Never executes anything.

use std::fmt;

#[derive(Debug)]
pub enum PeError {
    Truncated(&'static str),
    BadMagic(&'static str),
    Unsupported(&'static str),
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::Truncated(w) => write!(f, "file truncated while reading {w}"),
            PeError::BadMagic(w) => write!(f, "bad magic: {w}"),
            PeError::Unsupported(w) => write!(f, "unsupported: {w}"),
        }
    }
}

impl std::error::Error for PeError {}

#[derive(Debug)]
pub struct Section {
    pub name: String,
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

#[derive(Debug)]
pub struct Import {
    pub dll: String,
    /// RVA of this DLL's import address table (one u32 slot per function).
    pub iat_rva: u32,
    pub functions: Vec<String>,
}

#[derive(Debug)]
pub struct Export {
    pub name: Option<String>,
    pub ordinal: u32,
    pub rva: u32,
}

#[derive(Debug)]
pub struct Pe {
    pub is_dll: bool,
    pub size_of_headers: u32,
    /// RVAs of 32-bit absolute addresses that must be adjusted when the image moves.
    pub relocs: Vec<u32>,
    pub exports: Vec<Export>,
    pub image_base: u32,
    pub entry_rva: u32,
    pub size_of_image: u32,
    pub subsystem: u16,
    pub sections: Vec<Section>,
    pub imports: Vec<Import>,
}

fn u16_at(d: &[u8], o: usize, what: &'static str) -> Result<u16, PeError> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(PeError::Truncated(what))
}

fn u32_at(d: &[u8], o: usize, what: &'static str) -> Result<u32, PeError> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(PeError::Truncated(what))
}

fn cstr_at(d: &[u8], o: usize) -> String {
    let tail = d.get(o..).unwrap_or(&[]);
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..end]).into_owned()
}

impl Pe {
    pub fn parse(data: &[u8]) -> Result<Pe, PeError> {
        if data.get(0..2) != Some(b"MZ") {
            return Err(PeError::BadMagic("missing MZ header"));
        }
        let pe_off = u32_at(data, 0x3c, "e_lfanew")? as usize;
        if data.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
            return Err(PeError::BadMagic("missing PE signature"));
        }

        let coff = pe_off + 4;
        let machine = u16_at(data, coff, "machine")?;
        if machine != 0x14c {
            return Err(PeError::Unsupported("only x86 (i386) images"));
        }
        let num_sections = u16_at(data, coff + 2, "section count")? as usize;
        let opt_size = u16_at(data, coff + 16, "optional header size")? as usize;
        let is_dll = u16_at(data, coff + 18, "characteristics")? & 0x2000 != 0;

        let opt = coff + 20;
        if u16_at(data, opt, "optional magic")? != 0x10b {
            return Err(PeError::Unsupported("only PE32 (not PE32+)"));
        }
        let entry_rva = u32_at(data, opt + 16, "entry point")?;
        let image_base = u32_at(data, opt + 28, "image base")?;
        let size_of_image = u32_at(data, opt + 56, "size of image")?;
        let subsystem = u16_at(data, opt + 68, "subsystem")?;
        let size_of_headers = u32_at(data, opt + 60, "size of headers")?;
        let export_rva = u32_at(data, opt + 96, "export directory rva")?;
        let export_size = u32_at(data, opt + 100, "export directory size")?;
        let import_rva = u32_at(data, opt + 104, "import directory rva")?;
        let reloc_rva = u32_at(data, opt + 136, "reloc directory rva").unwrap_or(0);
        let reloc_size = u32_at(data, opt + 140, "reloc directory size").unwrap_or(0);

        let sec_table = opt + opt_size;
        let mut sections = Vec::with_capacity(num_sections);
        for i in 0..num_sections {
            let s = sec_table + i * 40;
            let name_bytes = data.get(s..s + 8).ok_or(PeError::Truncated("section name"))?;
            let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(8);
            sections.push(Section {
                name: String::from_utf8_lossy(&name_bytes[..end]).into_owned(),
                virtual_size: u32_at(data, s + 8, "section vsize")?,
                virtual_address: u32_at(data, s + 12, "section va")?,
                raw_size: u32_at(data, s + 16, "section raw size")?,
                raw_offset: u32_at(data, s + 20, "section raw offset")?,
                characteristics: u32_at(data, s + 36, "section flags")?,
            });
        }

        let mut pe = Pe {
            is_dll,
            size_of_headers,
            relocs: Vec::new(),
            exports: Vec::new(),
            image_base,
            entry_rva,
            size_of_image,
            subsystem,
            sections,
            imports: Vec::new(),
        };
        pe.imports = pe.read_imports(data, import_rva);
        pe.relocs = pe.read_relocs(data, reloc_rva, reloc_size);
        pe.exports = pe.read_exports(data, export_rva, export_size);
        Ok(pe)
    }

    /// Translate an RVA to a file offset using the section table.
    pub fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        self.sections.iter().find_map(|s| {
            let size = s.virtual_size.max(s.raw_size);
            (rva >= s.virtual_address && rva < s.virtual_address + size)
                .then(|| (rva - s.virtual_address + s.raw_offset) as usize)
        })
    }

    fn read_relocs(&self, data: &[u8], rva: u32, size: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let Some(start) = (rva != 0).then(|| self.rva_to_offset(rva)).flatten() else {
            return out;
        };
        let end = start + size as usize;
        let mut off = start;
        while off + 8 <= end {
            let (Ok(page), Ok(block)) = (u32_at(data, off, ""), u32_at(data, off + 4, "")) else {
                break;
            };
            if block < 8 {
                break;
            }
            for i in 0..(block as usize - 8) / 2 {
                let Ok(entry) = u16_at(data, off + 8 + 2 * i, "") else { break };
                if entry >> 12 == 3 {
                    out.push(page + (entry & 0xfff) as u32); // IMAGE_REL_BASED_HIGHLOW
                }
            }
            off += block as usize;
        }
        out
    }

    fn read_exports(&self, data: &[u8], rva: u32, size: u32) -> Vec<Export> {
        let mut out = Vec::new();
        let Some(d) = (rva != 0).then(|| self.rva_to_offset(rva)).flatten() else {
            return out;
        };
        let rd = |o: usize| u32_at(data, o, "").unwrap_or(0);
        let (ord_base, n_funcs, n_names) = (rd(d + 16), rd(d + 20) as usize, rd(d + 24) as usize);
        let (funcs, names, ords) = (
            self.rva_to_offset(rd(d + 28)),
            self.rva_to_offset(rd(d + 32)),
            self.rva_to_offset(rd(d + 36)),
        );
        let (Some(funcs), Some(names), Some(ords)) = (funcs, names, ords) else {
            return out;
        };
        let mut name_of = std::collections::HashMap::new();
        for i in 0..n_names {
            let idx = u16_at(data, ords + 2 * i, "").unwrap_or(0) as usize;
            if let Some(o) = self.rva_to_offset(rd(names + 4 * i)) {
                name_of.insert(idx, cstr_at(data, o));
            }
        }
        for i in 0..n_funcs {
            let frva = rd(funcs + 4 * i);
            if frva == 0 || (frva >= rva && frva < rva + size) {
                continue; // unused slot or forwarder
            }
            out.push(Export { name: name_of.remove(&i), ordinal: ord_base + i as u32, rva: frva });
        }
        out
    }

    fn read_imports(&self, data: &[u8], import_rva: u32) -> Vec<Import> {
        let mut out = Vec::new();
        let Some(mut off) = (import_rva != 0).then(|| self.rva_to_offset(import_rva)).flatten()
        else {
            return out;
        };
        loop {
            let (Ok(thunks), Ok(name_rva), Ok(first_thunk)) = (
                u32_at(data, off, ""),
                u32_at(data, off + 12, ""),
                u32_at(data, off + 16, ""),
            ) else {
                break;
            };
            if name_rva == 0 {
                break;
            }
            let dll = self
                .rva_to_offset(name_rva)
                .map(|o| cstr_at(data, o))
                .unwrap_or_default();
            let lookup = if thunks != 0 { thunks } else { first_thunk };
            let mut functions = Vec::new();
            if let Some(mut t) = self.rva_to_offset(lookup) {
                while let Ok(entry) = u32_at(data, t, "") {
                    if entry == 0 {
                        break;
                    }
                    if entry & 0x8000_0000 != 0 {
                        functions.push(format!("#{}", entry & 0xffff));
                    } else if let Some(o) = self.rva_to_offset(entry) {
                        functions.push(cstr_at(data, o + 2)); // skip 2-byte hint
                    }
                    t += 4;
                }
            }
            out.push(Import { dll, iat_rva: first_thunk, functions });
            off += 20;
        }
        out
    }
}
