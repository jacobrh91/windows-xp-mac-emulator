//! Debugging aids: `XP_TRACE_AT` and `XP_COVER` (see the functions).

use super::super::{ESP, Emu};

/// XP_COVER=<lo>-<hi> (hex): the address range whose blocks are reported at their first run,
/// and kept out of compiled code.
pub(crate) fn cover_range() -> Option<(u32, u32)> {
    static R: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *R.get_or_init(|| {
        let v = std::env::var("XP_COVER").ok()?;
        let (a, b) = v.split_once('-')?;
        Some((
            u32::from_str_radix(a.trim(), 16).ok()?,
            u32::from_str_radix(b.trim(), 16).ok()?,
        ))
    })
}

/// XP_TRACE_AT=<hex>,...: addresses whose blocks are logged each time they run (and kept out
/// of compiled code, so that every run passes through here).
pub(crate) fn trace_at(eip: u32) -> bool {
    static AT: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let at = AT.get_or_init(|| {
        std::env::var("XP_TRACE_AT")
            .map(|v| {
                v.split(',')
                    .filter_map(|a| u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).ok())
                    .collect()
            })
            .unwrap_or_default()
    });
    !at.is_empty() && at.contains(&eip)
}

impl Emu {
    /// XP_TRACE_AT: the registers, the top of the stack (return address, then the first
    /// arguments) and the dword ecx points to where a traced block starts.
    #[cold]
    pub(super) fn trace_here(&self, at: u32) {
        let r = |a: u32| self.mem.read(a, 4).unwrap_or(0xDEAD_DEAD);
        let esp = self.regs[ESP];
        let regs: Vec<String> = self.regs.iter().map(|v| format!("{v:#x}")).collect();
        let stack: Vec<String> = (0..4)
            .map(|i| format!("{:#x}", r(esp.wrapping_add(4 * i))))
            .collect();
        // (*ecx: the vtable, when ecx is a C++ `this`.)
        eprintln!(
            "[trace] {at:#x} regs [{}] stack [{}] *ecx {:#x}",
            regs.join(" "),
            stack.join(" "),
            r(self.regs[1])
        );
    }

    /// XP_COVER: report the first run of each block in the range, with the time (debugging).
    #[cold]
    pub(super) fn cover_here(&self, at: u32) {
        thread_local! {
            static SEEN: std::cell::RefCell<std::collections::HashSet<u32>> = Default::default();
        }
        if SEEN.with(|s| s.borrow_mut().insert(at)) {
            eprintln!(
                "[cover] {:.1} {at:x}",
                self.win.elapsed_ms() as f64 / 1000.0
            );
        }
    }
}
