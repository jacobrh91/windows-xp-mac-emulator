//! What a run reports when it stops, or periodically while it runs (debugging and testing).

use std::time::{Duration, Instant};
use std::{env, fs};

use crate::cpu::{EAX, EBP, EBX, ECX, EDI, EDX, ESI, ESP, Emu};

/// XP_FPS=1: presented frames per second, every five seconds (with `XP_BLOCK_STATS=1`, the
/// instruction cache and JIT statistics, and with `XP_API_STATS=1` the calls per Windows
/// function). `mark` is (when, frames, steps) at the last report; `idle` the time spent
/// sleeping since.
pub fn fps(emu: &mut Emu, started: Instant, mark: &mut (Instant, u64, u64), idle: &mut Duration) {
    let frames = emu.win.d3d.frames();
    let secs = mark.0.elapsed().as_secs_f64();
    let mut gaps = emu
        .win
        .d3d
        .dev
        .as_mut()
        .map(|d| std::mem::take(&mut d.frame_times))
        .unwrap_or_default();
    gaps.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f32| {
        gaps.get(((gaps.len() as f32 - 1.0) * p) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    eprintln!(
        "[fps] {:.1} at {:.0}s ({:.0}M instructions/s, idle {:.0}%) frame ms: median {:.0}, 95% {:.0}, worst {:.0}",
        (frames - mark.1) as f64 / secs,
        started.elapsed().as_secs_f64(),
        (emu.steps - mark.2) as f64 / secs / 1e6,
        idle.as_secs_f64() * 100.0 / secs,
        pct(0.5),
        pct(0.95),
        pct(1.0)
    );
    if env::var_os("XP_BLOCK_STATS").is_some() {
        eprintln!(
            "[blocks] {} built, {} runs ({:.1} instructions per run), {} flushes",
            emu.icache.built,
            emu.icache.runs,
            (emu.steps - mark.2) as f64 / emu.icache.runs.max(1) as f64,
            emu.icache.flushes
        );
        emu.icache.flushes = 0;
        let mut kinds: Vec<_> = emu.jit.helper_kinds.drain().collect();
        kinds.sort_by(|a, b| b.1.cmp(&a.1));
        let total = (emu.steps - mark.2).max(1) as f64;
        eprintln!(
            "[jit] {} compiled, {} block runs, {:.1}% of instructions via the interpreter: {}",
            emu.jit.compiled,
            emu.jit.runs,
            emu.jit.helper_calls as f64 * 100.0 / total,
            kinds
                .iter()
                .map(|(k, n)| format!("{k} {:.1}%", *n as f64 * 100.0 / total))
                .collect::<Vec<_>>()
                .join(", ")
        );
        eprintln!(
            "[chain] {} chained, {} back to the run loop",
            emu.jit_exits[0], emu.jit_exits[1]
        );
        emu.jit_exits = [0; 2];
        eprintln!(
            "[exits] budget {}, api {}, undecodable {}, not compiled {}, chain miss {}",
            emu.exit_why[0], emu.exit_why[1], emu.exit_why[2], emu.exit_why[3], emu.exit_why[4]
        );
        emu.exit_why = [0; 5];
        emu.jit.helper_calls = 0;
        emu.jit.runs = 0;
        emu.icache.built = 0;
        emu.icache.runs = 0;
    }
    if let Some(counts) = emu.win.api_counts.as_mut() {
        let mut top: Vec<(u64, usize)> = counts
            .iter()
            .enumerate()
            .filter(|c| *c.1 > 0)
            .map(|(i, &n)| (n, i))
            .collect();
        top.sort_by(|a, b| b.cmp(a));
        let names: Vec<String> = top
            .iter()
            .map(|&(n, i)| format!("{}!{} {n}", emu.win.stubs[i].dll, emu.win.stubs[i].name))
            .collect();
        eprintln!("[api] {}", names.join(", "));
        counts.iter_mut().for_each(|c| *c = 0);
    }
    *mark = (Instant::now(), frames, emu.steps);
    *idle = Duration::ZERO;
}

/// XP_PROFILE: where the guest spent its time, by opcode, module and address.
pub fn profile(emu: &mut Emu) {
    let Some(p) = emu.profile.take() else { return };
    let mut ops: Vec<(u32, u32)> = p
        .iter()
        .filter(|(k, _)| **k & 0x8000_0000 != 0)
        .map(|(k, v)| (*k & 0xFFFF, *v))
        .collect();
    ops.sort_by(|a, b| b.1.cmp(&a.1));
    let all: u64 = ops.iter().map(|o| o.1 as u64).sum();
    for (op, n) in ops.iter().take(60) {
        eprintln!(
            "[opcodes] {:5.1}% {}{:02x}",
            *n as f64 * 100.0 / all as f64,
            if *op >= 0x100 { "0f " } else { "" },
            op & 0xFF
        );
    }
    let mut rows: Vec<_> = p
        .into_iter()
        .filter(|(k, _)| *k & 0x8000_0000 == 0)
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    let total: u32 = rows.iter().map(|r| r.1).sum();
    let mut by_module: std::collections::HashMap<String, u32> = Default::default();
    for (addr, n) in &rows {
        let loc = emu.win.locate(*addr).unwrap_or_else(|| "?".into());
        *by_module
            .entry(loc.split('+').next().unwrap_or("?").to_string())
            .or_insert(0) += n;
    }
    let mut mods: Vec<_> = by_module.into_iter().collect();
    mods.sort_by(|a, b| b.1.cmp(&a.1));
    for (m, n) in mods.iter().take(8) {
        eprintln!("[module] {:5.1}% {m}", *n as f64 * 100.0 / total as f64);
    }
    let top = env::var("XP_PROFILE_TOP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(45);
    for (addr, n) in rows.iter().take(top) {
        eprintln!(
            "[profile] {:5.1}% {}",
            *n as f64 * 100.0 / total as f64,
            emu.win
                .locate(*addr)
                .unwrap_or_else(|| format!("{addr:#x}"))
        );
    }
}

/// Write the picture the live window would show (the content, without the emulated frame)
/// as a PPM file.
pub fn screenshot(emu: &mut Emu, out: &str) {
    crate::d3d::d3d8::finish_present(emu, true);
    let region = emu.wm.content_bounds();
    let desktop = emu.wm.compose();
    let shot = match region {
        Some((x, y, w, h)) => desktop.crop(x, y, w, h),
        None => desktop.crop(0, 0, desktop.width, desktop.height),
    };
    match fs::write(out, shot.to_ppm()) {
        Ok(()) => eprintln!("screenshot written to {out}"),
        Err(e) => eprintln!("cannot write {out}: {e}"),
    }
}

/// XP_REGS=1 and XP_PEEK=<hex address>:<length>[,...]: the guest's registers and memory
/// where it stopped.
pub fn registers_and_memory(emu: &Emu) {
    // XP_REGS=1: the guest's registers where it stopped.
    if env::var_os("XP_REGS").is_some() {
        let r = &emu.regs;
        eprintln!(
            "regs eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esp={:#x} ebp={:#x} esi={:#x} edi={:#x} eflags={:#x}",
            r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], emu.eflags
        );
    }
    // XP_PEEK=<hex address>:<length>[,...]: guest memory where it stopped.
    for spec in env::var("XP_PEEK")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
    {
        let (a, n) = spec.split_once(':').unwrap_or((spec, "64"));
        let (Ok(a), Ok(n)) = (
            u32::from_str_radix(a.trim_start_matches("0x"), 16),
            n.parse::<u32>(),
        ) else {
            continue;
        };
        for row in (0..n).step_by(16) {
            let bytes = emu
                .mem
                .read_bytes(a + row, 16.min(n - row))
                .unwrap_or_default();
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            eprintln!("{:08x}: {}", a + row, hex.join(" "));
        }
    }
}

/// Where did it go wrong? Registers, the module the crash is in, and return addresses that
/// point into game code (found by scanning the stack), so the spot can be looked up.
pub fn crash(emu: &Emu) {
    let r = emu.regs;
    eprintln!(
        "[crash] eip={:#x} ({})  eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esp={:#x} ebp={:#x} esi={:#x} edi={:#x}",
        emu.eip,
        emu.win.locate(emu.eip).unwrap_or_else(|| "unknown".into()),
        r[EAX],
        r[ECX],
        r[EDX],
        r[EBX],
        r[ESP],
        r[EBP],
        r[ESI],
        r[EDI]
    );
    let mut shown = 0;
    for i in 0..256u32 {
        let Ok(v) = emu.mem.read(r[ESP].wrapping_add(4 * i), 4) else {
            break;
        };
        if let Some(loc) = emu.win.locate(v) {
            eprintln!("[crash]   stack+{:#04x}: {v:#x} = {loc}", 4 * i);
            shown += 1;
            if shown >= 12 {
                break;
            }
        }
    }
}
