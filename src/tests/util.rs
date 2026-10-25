//! Builds tiny but valid PE32 images so tests can exercise the loader and Win32 layer
//! without needing any real Windows binaries.

pub const IMAGE_BASE: u32 = 0x0040_0000;
const TEXT_RVA: u32 = 0x1000;
const IDATA_RVA: u32 = 0x2000;

pub struct Spec<'a> {
    pub imports: &'a [(&'a str, &'a [&'a str])],
    /// (export name, code offset of the function)
    pub exports: &'a [(&'a str, u32)],
    /// Code offsets of 32-bit absolute addresses that need base relocation.
    pub relocs: &'a [u32],
    pub dll: bool,
    pub base: u32,
}

impl Default for Spec<'_> {
    fn default() -> Self {
        Spec {
            imports: &[],
            exports: &[],
            relocs: &[],
            dll: false,
            base: IMAGE_BASE,
        }
    }
}

pub struct Layout {
    base: u32,
    iat: Vec<Vec<u32>>, // rva of each function's IAT slot, per dll
}

impl Layout {
    /// Virtual address of the IAT slot for `imports[dll][func]`, for `call [slot]`.
    pub fn iat_va(&self, dll: usize, func: usize) -> u32 {
        self.base + self.iat[dll][func]
    }

    /// Preferred virtual address of `offset` bytes into the code section.
    pub fn code_va(&self, offset: u32) -> u32 {
        self.base + TEXT_RVA + offset
    }
}

pub const CODE_VA: u32 = IMAGE_BASE + TEXT_RVA;

fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn pad_to(v: &mut Vec<u8>, align: usize) {
    while !v.len().is_multiple_of(align) {
        v.push(0);
    }
}

pub fn build_pe(imports: &[(&str, &[&str])], code: impl Fn(&Layout) -> Vec<u8>) -> Vec<u8> {
    build(
        &Spec {
            imports,
            ..Spec::default()
        },
        code,
    )
}

/// `code` is placed at RVA 0x1000 and is also the entry point. It may contain data
/// after the instructions, which is why `.text` is mapped writable by our loader.
pub fn build(spec: &Spec, code: impl Fn(&Layout) -> Vec<u8>) -> Vec<u8> {
    let imports = spec.imports;
    // ---- .idata layout: descriptors, then per-dll INT + IAT, then names ----
    let desc_bytes = (imports.len() + 1) * 20;
    let mut cursor = desc_bytes as u32;
    let mut int_rvas = Vec::new();
    let mut iat_rvas = Vec::new();
    for (_, funcs) in imports {
        int_rvas.push(IDATA_RVA + cursor);
        cursor += (funcs.len() as u32 + 1) * 4;
        iat_rvas.push(IDATA_RVA + cursor);
        cursor += (funcs.len() as u32 + 1) * 4;
    }
    let layout = Layout {
        base: spec.base,
        iat: imports
            .iter()
            .zip(&iat_rvas)
            .map(|((_, funcs), &base)| (0..funcs.len() as u32).map(|i| base + 4 * i).collect())
            .collect(),
    };

    let mut idata = vec![0u8; cursor as usize];
    let mut dll_name_rva = Vec::new();
    let mut thunks: Vec<Vec<u32>> = Vec::new();
    for (dll, funcs) in imports {
        dll_name_rva.push(IDATA_RVA + idata.len() as u32);
        idata.extend_from_slice(dll.as_bytes());
        idata.push(0);
        let mut t = Vec::new();
        for f in *funcs {
            if idata.len() % 2 == 1 {
                idata.push(0);
            }
            t.push(IDATA_RVA + idata.len() as u32);
            idata.extend_from_slice(&[0, 0]); // hint
            idata.extend_from_slice(f.as_bytes());
            idata.push(0);
        }
        thunks.push(t);
    }
    for (i, t) in thunks.iter().enumerate() {
        let int_off = (int_rvas[i] - IDATA_RVA) as usize;
        let iat_off = (iat_rvas[i] - IDATA_RVA) as usize;
        for (j, &rva) in t.iter().enumerate() {
            idata[int_off + 4 * j..int_off + 4 * j + 4].copy_from_slice(&rva.to_le_bytes());
            idata[iat_off + 4 * j..iat_off + 4 * j + 4].copy_from_slice(&rva.to_le_bytes());
        }
        let d = i * 20;
        idata[d..d + 4].copy_from_slice(&int_rvas[i].to_le_bytes());
        idata[d + 12..d + 16].copy_from_slice(&dll_name_rva[i].to_le_bytes());
        idata[d + 16..d + 20].copy_from_slice(&iat_rvas[i].to_le_bytes());
    }
    let import_size = idata.len() as u32;

    // ---- export directory ----
    let (mut export_rva, mut export_size) = (0, 0);
    if !spec.exports.is_empty() {
        pad_to(&mut idata, 4);
        let start = idata.len();
        export_rva = IDATA_RVA + start as u32;
        let n = spec.exports.len() as u32;
        let funcs_rva = export_rva + 40;
        let names_rva = funcs_rva + 4 * n;
        let ords_rva = names_rva + 4 * n;
        let strings_rva = ords_rva + 2 * n;
        let mut dir = vec![0u8; 40];
        dir[16..20].copy_from_slice(&1u32.to_le_bytes()); // ordinal base
        dir[20..24].copy_from_slice(&n.to_le_bytes());
        dir[24..28].copy_from_slice(&n.to_le_bytes());
        dir[28..32].copy_from_slice(&funcs_rva.to_le_bytes());
        dir[32..36].copy_from_slice(&names_rva.to_le_bytes());
        dir[36..40].copy_from_slice(&ords_rva.to_le_bytes());
        let mut funcs = Vec::new();
        let mut names = Vec::new();
        let mut ords = Vec::new();
        let mut strings = Vec::new();
        for (i, (name, off)) in spec.exports.iter().enumerate() {
            put32(&mut funcs, TEXT_RVA + off);
            put32(&mut names, strings_rva + strings.len() as u32);
            put16(&mut ords, i as u16);
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        idata.extend_from_slice(&dir);
        idata.extend_from_slice(&funcs);
        idata.extend_from_slice(&names);
        idata.extend_from_slice(&ords);
        idata.extend_from_slice(&strings);
        export_size = idata.len() as u32 - start as u32;
    }

    // ---- base relocations (one block: everything is in the page at TEXT_RVA) ----
    let (mut reloc_rva, mut reloc_size) = (0, 0);
    if !spec.relocs.is_empty() {
        pad_to(&mut idata, 4);
        reloc_rva = IDATA_RVA + idata.len() as u32;
        let mut block = Vec::new();
        put32(&mut block, TEXT_RVA);
        put32(&mut block, 8 + 2 * spec.relocs.len() as u32);
        for off in spec.relocs {
            assert!(*off < 0x1000);
            put16(&mut block, (3 << 12) | *off as u16);
        }
        reloc_size = block.len() as u32;
        idata.extend_from_slice(&block);
    }
    let idata_vsize = idata.len() as u32;
    pad_to(&mut idata, 0x200);

    let mut text = code(&layout);
    let text_vsize = text.len() as u32;
    pad_to(&mut text, 0x200);

    // ---- headers ----
    let mut f = vec![0u8; 0x40];
    f[0] = b'M';
    f[1] = b'Z';
    f[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    f.extend_from_slice(b"PE\0\0");
    put16(&mut f, 0x14c); // machine
    put16(&mut f, 2); // sections
    put32(&mut f, 0);
    put32(&mut f, 0);
    put32(&mut f, 0);
    put16(&mut f, 224); // optional header size
    put16(&mut f, if spec.dll { 0x2102 } else { 0x0102 });

    let opt_start = f.len();
    f.resize(opt_start + 224, 0);
    let set32 = |f: &mut Vec<u8>, off: usize, v: u32| {
        f[opt_start + off..opt_start + off + 4].copy_from_slice(&v.to_le_bytes())
    };
    f[opt_start..opt_start + 2].copy_from_slice(&0x10bu16.to_le_bytes());
    set32(&mut f, 16, TEXT_RVA); // entry point
    set32(&mut f, 28, spec.base);
    set32(&mut f, 32, 0x1000); // section alignment
    set32(&mut f, 36, 0x200); // file alignment
    set32(&mut f, 56, 0x3000); // size of image
    set32(&mut f, 60, 0x200); // size of headers
    f[opt_start + 68] = 3; // console subsystem
    set32(&mut f, 92, 16); // data directories
    set32(&mut f, 96, export_rva);
    set32(&mut f, 100, export_size);
    set32(&mut f, 104, IDATA_RVA); // import directory
    set32(&mut f, 108, import_size);
    set32(&mut f, 136, reloc_rva);
    set32(&mut f, 140, reloc_size);

    let section =
        |f: &mut Vec<u8>, name: &[u8], vsize: u32, rva: u32, raw: u32, ptr: u32, flags: u32| {
            let mut n = [0u8; 8];
            n[..name.len()].copy_from_slice(name);
            f.extend_from_slice(&n);
            put32(f, vsize);
            put32(f, rva);
            put32(f, raw);
            put32(f, ptr);
            f.extend_from_slice(&[0; 12]);
            put32(f, flags);
        };
    section(
        &mut f,
        b".text",
        text_vsize,
        TEXT_RVA,
        text.len() as u32,
        0x200,
        0xE000_0020,
    );
    section(
        &mut f,
        b".idata",
        idata_vsize,
        IDATA_RVA,
        idata.len() as u32,
        0x200 + text.len() as u32,
        0xC000_0040,
    );
    f.resize(0x200, 0);
    f.extend_from_slice(&text);
    f.extend_from_slice(&idata);
    f
}

/// Code-section offset where tests keep scratch variables ("data slots").
pub const DATA_OFFSET: u32 = 0x300;

/// Virtual address of 4-byte data slot `slot`.
pub fn data_va(slot: u32) -> u32 {
    CODE_VA + DATA_OFFSET + 4 * slot
}

/// A small x86 assembler for hand-written test programs: raw bytes plus labels, rel8/rel32
/// branches and absolute label addresses.
#[derive(Default)]
pub struct Asm {
    b: Vec<u8>,
    labels: std::collections::HashMap<&'static str, u32>,
    abs: Vec<(usize, &'static str)>,
    rel8: Vec<(usize, &'static str)>,
    rel32: Vec<(usize, &'static str)>,
}

impl Asm {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn e(&mut self, bytes: &[u8]) -> &mut Self {
        self.b.extend_from_slice(bytes);
        self
    }
    pub fn label(&mut self, n: &'static str) -> &mut Self {
        self.labels.insert(n, self.b.len() as u32);
        self
    }
    pub fn call(&mut self, slot: u32) -> &mut Self {
        self.e(&[0xFF, 0x15]);
        self.b.extend_from_slice(&slot.to_le_bytes());
        self
    }
    /// A 32-bit immediate holding the address of data slot `slot`.
    pub fn data_va(&mut self, slot: u32) -> &mut Self {
        self.b.extend_from_slice(&data_va(slot).to_le_bytes());
        self
    }
    /// mov [slot], eax
    pub fn store(&mut self, slot: u32) -> &mut Self {
        self.e(&[0xA3]).data_va(slot)
    }
    /// push dword [slot]
    pub fn push_mem_data(&mut self, slot: u32) -> &mut Self {
        self.e(&[0xFF, 0x35]).data_va(slot)
    }
    /// push <address of label>
    pub fn push_label(&mut self, n: &'static str) -> &mut Self {
        self.e(&[0x68]);
        self.abs.push((self.b.len(), n));
        self.b.extend_from_slice(&[0; 4]);
        self
    }
    pub fn jcc8(&mut self, op: u8, n: &'static str) -> &mut Self {
        self.e(&[op, 0]);
        self.rel8.push((self.b.len() - 1, n));
        self
    }
    pub fn jmp8(&mut self, n: &'static str) -> &mut Self {
        self.jcc8(0xEB, n)
    }
    pub fn call_label(&mut self, n: &'static str) -> &mut Self {
        self.e(&[0xE8]);
        self.rel32.push((self.b.len(), n));
        self.b.extend_from_slice(&[0; 4]);
        self
    }
    pub fn finish(mut self) -> Vec<u8> {
        for (pos, n) in std::mem::take(&mut self.abs) {
            let v = CODE_VA + self.labels[n];
            self.b[pos..pos + 4].copy_from_slice(&v.to_le_bytes());
        }
        for (pos, n) in std::mem::take(&mut self.rel8) {
            let rel = self.labels[n] as i64 - (pos as i64 + 1);
            assert!((-128..128).contains(&rel), "rel8 out of range to {n}");
            self.b[pos] = rel as i8 as u8;
        }
        for (pos, n) in std::mem::take(&mut self.rel32) {
            let rel = self.labels[n] as i64 - (pos as i64 + 4);
            self.b[pos..pos + 4].copy_from_slice(&(rel as i32).to_le_bytes());
        }
        assert!(
            self.b.len() <= DATA_OFFSET as usize,
            "code overlaps the data area"
        );
        self.b.resize(DATA_OFFSET as usize + 0x100, 0);
        self.b
    }
}
