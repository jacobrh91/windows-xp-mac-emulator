//! Sparse 32-bit guest address space. Pages are allocated on `map`;
//! touching an unmapped page is a `Fault`.

const PAGE_BITS: u32 = 12;
const PAGE_SIZE: usize = 1 << PAGE_BITS;
const PAGE_COUNT: usize = 1 << (32 - PAGE_BITS);

type Page = Box<[u8; PAGE_SIZE]>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault(pub u32);

pub struct Memory {
    pages: Vec<Option<Page>>,
    /// Debugging: log writes touching this address (see `take_watch_hits`).
    pub watch: Option<u32>,
    /// Whether the most recent failed access was a write (for access-violation reports).
    pub fault_write: std::cell::Cell<bool>,
    watch_hits: Vec<(u32, u32, u32)>,
}

impl Memory {
    pub fn new() -> Self {
        let mut pages = Vec::new();
        pages.resize_with(PAGE_COUNT, || None);
        Memory { pages, watch: None, fault_write: std::cell::Cell::new(false), watch_hits: Vec::new() }
    }

    /// Make `[addr, addr + size)` accessible (zero-filled). Already-mapped pages are kept.
    /// The first mapped page address in `[addr, addr + size)`, if any.
    pub fn first_mapped(&self, addr: u32, size: u32) -> Option<u32> {
        let first = (addr >> PAGE_BITS) as usize;
        let last = (((addr as u64) + (size as u64).max(1) - 1) >> PAGE_BITS) as usize;
        (first..=last.min(PAGE_COUNT - 1)).find(|&p| self.pages[p].is_some()).map(|p| (p as u32) << PAGE_BITS)
    }

    pub fn map(&mut self, addr: u32, size: u32) {
        if size == 0 {
            return;
        }
        let first = (addr >> PAGE_BITS) as usize;
        let last = (((addr as u64) + (size as u64) - 1) >> PAGE_BITS) as usize;
        for p in first..=last.min(PAGE_COUNT - 1) {
            if self.pages[p].is_none() {
                self.pages[p] = Some(Box::new([0; PAGE_SIZE]));
            }
        }
    }

    /// Release `[addr, addr + size)`; touching it afterwards faults (like a freed mapping).
    pub fn unmap(&mut self, addr: u32, size: u32) {
        if size == 0 {
            return;
        }
        let first = (addr >> PAGE_BITS) as usize;
        let last = (((addr as u64) + (size as u64) - 1) >> PAGE_BITS) as usize;
        for p in first..=last.min(PAGE_COUNT - 1) {
            self.pages[p] = None;
        }
    }

    pub fn is_mapped(&self, addr: u32) -> bool {
        self.pages[(addr >> PAGE_BITS) as usize].is_some()
    }

    #[inline(always)]
    fn page(&self, addr: u32) -> Result<&[u8; PAGE_SIZE], Fault> {
        // SAFETY: `pages` has PAGE_COUNT entries and addr >> PAGE_BITS < PAGE_COUNT.
        match unsafe { self.pages.get_unchecked((addr >> PAGE_BITS) as usize) } {
            Some(p) => Ok(p),
            None => Err(self.read_fault(addr)),
        }
    }

    #[cold]
    #[inline(never)]
    fn read_fault(&self, addr: u32) -> Fault {
        self.fault_write.set(false);
        Fault(addr)
    }

    #[inline(always)]
    fn page_mut(&mut self, addr: u32) -> Result<&mut [u8; PAGE_SIZE], Fault> {
        // SAFETY: as in `page`.
        match unsafe { self.pages.get_unchecked_mut((addr >> PAGE_BITS) as usize) } {
            Some(p) => Ok(p),
            None => {
                self.fault_write.set(true);
                Err(Fault(addr))
            }
        }
    }

    #[inline(always)]
    pub fn read_u8(&self, addr: u32) -> Result<u8, Fault> {
        Ok(self.page(addr)?[(addr as usize) & (PAGE_SIZE - 1)])
    }

    /// Little-endian read of `n` bytes (1, 2 or 4).
    #[inline(always)]
    pub fn read(&self, addr: u32, n: u32) -> Result<u32, Fault> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        let mut v = 0u32;
        if off + n as usize <= PAGE_SIZE {
            let pg = self.page(addr)?;
            return Ok(match n {
                4 => u32::from_le_bytes([pg[off], pg[off + 1], pg[off + 2], pg[off + 3]]),
                2 => u16::from_le_bytes([pg[off], pg[off + 1]]) as u32,
                1 => pg[off] as u32,
                _ => {
                    for i in 0..n as usize {
                        v |= (pg[off + i] as u32) << (8 * i);
                    }
                    v
                }
            });
        } else {
            for i in 0..n {
                v |= (self.read_u8(addr.wrapping_add(i))? as u32) << (8 * i);
            }
        }
        Ok(v)
    }

    /// Little-endian write of the low `n` bytes (1, 2 or 4) of `val`.
    /// 64-bit little-endian read (MMX operands).
    #[inline(always)]
    pub fn read64(&self, addr: u32) -> Result<u64, Fault> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off + 8 <= PAGE_SIZE {
            let pg = self.page(addr)?;
            return Ok(u64::from_le_bytes(pg[off..off + 8].try_into().unwrap()));
        }
        Ok(self.read(addr, 4)? as u64 | (self.read(addr.wrapping_add(4), 4)? as u64) << 32)
    }

    #[inline(always)]
    pub fn write64(&mut self, addr: u32, v: u64) -> Result<(), Fault> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off + 8 <= PAGE_SIZE && self.watch.is_none() {
            self.page_mut(addr)?[off..off + 8].copy_from_slice(&v.to_le_bytes());
            return Ok(());
        }
        self.write(addr, 4, v as u32)?;
        self.write(addr.wrapping_add(4), 4, (v >> 32) as u32)
    }

    #[cold]
    #[inline(never)]
    fn note_watch(&mut self, addr: u32, n: u32, val: u32) {
        if let Some(w) = self.watch {
            if addr <= w && w < addr.wrapping_add(n) {
                self.watch_hits.push((addr, n, val));
            }
        }
    }

    #[inline(always)]
    pub fn write(&mut self, addr: u32, n: u32, val: u32) -> Result<(), Fault> {
        if self.watch.is_some() {
            self.note_watch(addr, n, val);
        }
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off + n as usize <= PAGE_SIZE {
            let pg = self.page_mut(addr)?;
            match n {
                4 => pg[off..off + 4].copy_from_slice(&val.to_le_bytes()),
                2 => pg[off..off + 2].copy_from_slice(&(val as u16).to_le_bytes()),
                1 => pg[off] = val as u8,
                _ => {
                    for i in 0..n as usize {
                        pg[off + i] = (val >> (8 * i)) as u8;
                    }
                }
            }
        } else {
            for i in 0..n {
                let a = addr.wrapping_add(i);
                self.page_mut(a)?[(a as usize) & (PAGE_SIZE - 1)] = (val >> (8 * i)) as u8;
            }
        }
        Ok(())
    }

    /// Read a NUL-terminated UTF-16 string, capped at 64K characters.
    pub fn read_wstr(&self, addr: u32) -> Result<String, Fault> {
        let mut units = Vec::new();
        for i in 0..0x10000u32 {
            let c = self.read(addr.wrapping_add(2 * i), 2)? as u16;
            if c == 0 {
                break;
            }
            units.push(c);
        }
        Ok(String::from_utf16_lossy(&units))
    }

    pub fn write_wstr(&mut self, addr: u32, s: &str) -> Result<(), Fault> {
        for (i, c) in s.encode_utf16().chain(std::iter::once(0)).enumerate() {
            self.write(addr.wrapping_add(2 * i as u32), 2, c as u32)?;
        }
        Ok(())
    }

    /// Writes to the watched address since the last call: (address, width, value).
    pub fn take_watch_hits(&mut self) -> Vec<(u32, u32, u32)> {
        std::mem::take(&mut self.watch_hits)
    }

    pub fn read_bytes(&self, addr: u32, len: u32) -> Result<Vec<u8>, Fault> {
        // Whole pages at a time.
        let mut out = Vec::with_capacity(len as usize);
        while (out.len() as u32) < len {
            let a = addr.wrapping_add(out.len() as u32);
            let off = (a as usize) & (PAGE_SIZE - 1);
            let n = (PAGE_SIZE - off).min(len as usize - out.len());
            out.extend_from_slice(&self.page(a)?[off..off + n]);
        }
        Ok(out)
    }

    pub fn write_bytes(&mut self, addr: u32, data: &[u8]) -> Result<(), Fault> {
        if self.watch.is_none() {
            // Whole pages at a time.
            let mut done = 0usize;
            while done < data.len() {
                let a = addr.wrapping_add(done as u32);
                let off = (a as usize) & (PAGE_SIZE - 1);
                let n = (PAGE_SIZE - off).min(data.len() - done);
                self.page_mut(a)?[off..off + n].copy_from_slice(&data[done..done + n]);
                done += n;
            }
            return Ok(());
        }
        for (i, b) in data.iter().enumerate() {
            self.write(addr.wrapping_add(i as u32), 1, *b as u32)?;
        }
        Ok(())
    }

    /// Read a NUL-terminated string (lossy UTF-8), capped at 64 KiB.
    pub fn read_cstr(&self, addr: u32) -> Result<String, Fault> {
        let mut out = Vec::new();
        for i in 0..0x10000u32 {
            let b = self.read_u8(addr.wrapping_add(i))?;
            if b == 0 {
                break;
            }
            out.push(b);
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmapped_access_faults() {
        let mut m = Memory::new();
        assert_eq!(m.read(0x1000, 4), Err(Fault(0x1000)));
        assert_eq!(m.write(0x1000, 4, 1), Err(Fault(0x1000)));
    }

    #[test]
    fn little_endian_roundtrip() {
        let mut m = Memory::new();
        m.map(0x1000, 0x1000);
        m.write(0x1000, 4, 0xaabbccdd).unwrap();
        assert_eq!(m.read_u8(0x1000).unwrap(), 0xdd);
        assert_eq!(m.read(0x1000, 2).unwrap(), 0xccdd);
        assert_eq!(m.read(0x1000, 4).unwrap(), 0xaabbccdd);
    }

    #[test]
    fn access_across_page_boundary() {
        let mut m = Memory::new();
        m.map(0x1000, 0x2000);
        m.write(0x1ffe, 4, 0x11223344).unwrap();
        assert_eq!(m.read(0x1ffe, 4).unwrap(), 0x11223344);
        assert_eq!(m.read_u8(0x2001).unwrap(), 0x11);
    }

    #[test]
    fn cross_boundary_into_unmapped_faults() {
        let mut m = Memory::new();
        m.map(0x1000, 0x1000);
        assert_eq!(m.write(0x1ffe, 4, 0), Err(Fault(0x2000)));
    }

    #[test]
    fn map_is_idempotent_and_keeps_data() {
        let mut m = Memory::new();
        m.map(0x1000, 4);
        m.write(0x1000, 1, 0x7f).unwrap();
        m.map(0x1000, 0x1000);
        assert_eq!(m.read_u8(0x1000).unwrap(), 0x7f);
    }

    #[test]
    fn cstr_and_bytes() {
        let mut m = Memory::new();
        m.map(0x4000, 0x100);
        m.write_bytes(0x4000, b"hello\0junk").unwrap();
        assert_eq!(m.read_cstr(0x4000).unwrap(), "hello");
        assert_eq!(m.read_bytes(0x4000, 3).unwrap(), b"hel");
    }

    #[test]
    fn top_of_address_space_maps() {
        let mut m = Memory::new();
        m.map(0xffff_f000, 0x1000);
        assert!(m.is_mapped(0xffff_ffff));
    }
}
