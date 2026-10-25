mod cpu;
mod d3d;
mod gui;
mod hooks;
mod loader;
mod media;
mod memory;
mod win32;

#[cfg(test)]
mod tests;

use std::{env, fs, process};

const USAGE: &str = "usage: windows-xp-mac-emulator info <file.exe>\n       windows-xp-mac-emulator run [--window] [--fullscreen] [--screenshot out.ppm] <file.exe>";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("info") if args.len() == 2 => info(&args[1], &read(&args[1])),
        Some("run") => {
            let (mut window, mut fullscreen, mut shot, mut path) = (false, false, None, None);
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--window" => window = true,
                    "--fullscreen" => (window, fullscreen) = (true, true),
                    "--screenshot" => shot = it.next().cloned(),
                    _ => path = Some(a.clone()),
                }
            }
            let Some(path) = path else {
                eprintln!("{USAGE}");
                process::exit(2);
            };
            run(&path, &read(&path), window, fullscreen, shot);
        }
        _ => {
            eprintln!("{USAGE}");
            process::exit(2);
        }
    }
}

fn read(path: &str) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        process::exit(1);
    })
}

fn info(path: &str, data: &[u8]) {
    let pe = loader::pe::Pe::parse(data).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        process::exit(1);
    });
    println!("image base   0x{:08x}", pe.image_base);
    println!("entry point  0x{:08x} (rva)", pe.entry_rva);
    println!("image size   0x{:08x}", pe.size_of_image);
    println!("subsystem    {}", pe.subsystem);
    println!("sections:");
    for s in &pe.sections {
        println!(
            "  {:<8} va=0x{:08x} vsize=0x{:08x} raw=0x{:08x}+0x{:x} flags=0x{:08x}",
            s.name, s.virtual_address, s.virtual_size, s.raw_offset, s.raw_size, s.characteristics
        );
    }
    println!("imports:");
    for i in &pe.imports {
        let missing = i
            .functions
            .iter()
            .filter(|f| !win32::Win32::is_supported(&i.dll, f))
            .count();
        println!("  {} ({} functions, {} not yet emulated)", i.dll, i.functions.len(), missing);
        if env::var_os("SHOW_MISSING").is_some() {
            for f in i.functions.iter().filter(|f| !win32::Win32::is_supported(&i.dll, f)) {
                println!("      missing: {f}");
            }
        }
    }
}

/// Runs the program inside the emulator only; nothing in the image executes natively.
fn run(path: &str, data: &[u8], window: bool, fullscreen: bool, screenshot: Option<String>) {
    let mut emu = cpu::Emu::new();
    if let Some(dir) = std::path::Path::new(path).parent() {
        emu.win.dll_dirs.push(dir.to_path_buf());
        // The guest sees the program's folder as C:\Game, and nothing else on this machine.
        let dir = if dir.as_os_str().is_empty() { std::path::Path::new(".") } else { dir };
        emu.win.vfs.mount(r"C:\Game", dir);
        emu.win.vfs.cwd = r"C:\Game".to_string();
        let file = std::path::Path::new(path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        emu.win.exe_path = format!(r"C:\Game\{file}");
    }
    emu.win.trace = env::var_os("TRACE_API").is_some();
    if env::var_os("XP_SKIP_MOVIES").is_some() {
        emu.win.vfs.hidden_ext.push(".bik".to_string()); // Bink videos: the game carries on without them
    }
    // XP_PROFILE=1 profiles the whole run; XP_PROFILE=<seconds> starts that far in.
    let profile_from: Option<u64> = env::var("XP_PROFILE").ok().map(|v| v.parse().unwrap_or(0));
    emu.mem.watch = env::var("XP_WATCH").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let image = loader::load(&mut emu, data).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        process::exit(1);
    });
    eprintln!(
        "loaded at {:#x}, entry {:#x}, {} unsupported imports",
        image.base,
        image.entry,
        image.unsupported.len()
    );
    let mut presenter = window.then(|| {
        gui::present::Presenter::new("Windows XP emulator", fullscreen).unwrap_or_else(|e| {
            eprintln!("cannot open window: {e}");
            process::exit(1);
        })
    });

    // XP_MAX_STEPS stops a run early (with the screenshot) for reproducible checks.
    let max_steps: u64 = env::var("XP_MAX_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000_000_000);
    let mut closing_waits = 0;
    let dump_after = env::var("XP_DUMP_AFTER").ok().and_then(|v| v.parse::<u64>().ok()).map(std::time::Duration::from_secs);
    let started = std::time::Instant::now();
    let mut dumped = false;
    let mut tapped = 0u128;
    let show_fps = env::var_os("XP_FPS").is_some();
    let mut fps_mark = (std::time::Instant::now(), 0u64, 0u64);
    let mut idle = std::time::Duration::ZERO;
    // XP_CLICKS=<secs>:<x>:<y>,...  scripted left clicks at desktop coordinates (headless testing).
    let mut clicks: Vec<(u64, i32, i32)> = env::var("XP_CLICKS")
        .map(|v| v.split(',').filter_map(|c| {
            let mut p = c.split(':').map(|n| n.trim().parse::<i64>().ok());
            Some((p.next()?? as u64, p.next()?? as i32, p.next()?? as i32))
        }).collect())
        .unwrap_or_default();
    clicks.reverse();
    // XP_KEYS=<secs>:<virtual key>,...  scripted key presses (27 is Esc).
    let mut keys: Vec<(u64, u32)> = env::var("XP_KEYS")
        .map(|v| v.split(',').filter_map(|k| {
            let (t, vk) = k.split_once(':')?;
            Some((t.trim().parse().ok()?, vk.trim().parse().ok()?))
        }).collect())
        .unwrap_or_default();
    keys.reverse();
    let stop = loop {
        let stop = emu.run(1_000_000);
        if let Some(s) = env::var("XP_STOP_AFTER").ok().and_then(|v| v.parse::<u64>().ok())
            && started.elapsed().as_secs() >= s {
                break stop;
            }
        // XP_TAP=<vk>: tap that key every two seconds (headless way to skip intro movies).
        if let Some(vk) = env::var("XP_TAP").ok().and_then(|v| v.parse::<u32>().ok())
            && started.elapsed().as_millis() / 2000 != tapped {
                tapped = started.elapsed().as_millis() / 2000;
                emu.wm.post_key(vk, true);
                emu.wm.post_key(vk, false);
            }
        if profile_from.is_some_and(|t| started.elapsed().as_secs() >= t) && emu.profile.is_none() {
            emu.profile = Some(Default::default());
        }
        // XP_FPS=1: report presented frames per second every 5 seconds.
        if show_fps && fps_mark.0.elapsed().as_secs() >= 5 {
            let frames = emu.win.d3d.frames();
            let secs = fps_mark.0.elapsed().as_secs_f64();
            let mut gaps = emu.win.d3d.dev.as_mut().map(|d| std::mem::take(&mut d.frame_times)).unwrap_or_default();
            gaps.sort_by(|a, b| a.total_cmp(b));
            let pct = |p: f32| gaps.get(((gaps.len() as f32 - 1.0) * p) as usize).copied().unwrap_or(0.0);
            eprintln!(
                "[fps] {:.1} at {:.0}s ({:.0}M instructions/s, idle {:.0}%) frame ms: median {:.0}, 95% {:.0}, worst {:.0}",
                (frames - fps_mark.1) as f64 / secs,
                started.elapsed().as_secs_f64(),
                (emu.steps - fps_mark.2) as f64 / secs / 1e6,
                idle.as_secs_f64() * 100.0 / secs,
                pct(0.5),
                pct(0.95),
                pct(1.0)
            );
            if env::var_os("XP_BLOCK_STATS").is_some() {
                eprintln!("[blocks] {} built, {} runs ({:.1} instructions per run), {} flushes", emu.icache.built, emu.icache.runs, (emu.steps - fps_mark.2) as f64 / emu.icache.runs.max(1) as f64, emu.icache.flushes);
                emu.icache.flushes = 0;
                let mut kinds: Vec<_> = emu.jit.helper_kinds.drain().collect();
                kinds.sort_by(|a, b| b.1.cmp(&a.1));
                let total = (emu.steps - fps_mark.2).max(1) as f64;
                eprintln!(
                    "[jit] {} compiled, {} block runs, {:.1}% of instructions via the interpreter: {}",
                    emu.jit.compiled,
                    emu.jit.runs,
                    emu.jit.helper_calls as f64 * 100.0 / total,
                    kinds.iter().map(|(k, n)| format!("{k} {:.1}%", *n as f64 * 100.0 / total)).collect::<Vec<_>>().join(", ")
                );
                emu.jit.helper_calls = 0;
                emu.jit.runs = 0;
                emu.icache.built = 0;
                emu.icache.runs = 0;
            }
            fps_mark = (std::time::Instant::now(), frames, emu.steps);
            idle = std::time::Duration::ZERO;
        }
        while keys.last().is_some_and(|k| started.elapsed().as_secs() >= k.0) {
            let (_, vk) = keys.pop().unwrap();
            emu.wm.post_key(vk, true);
            emu.wm.post_key(vk, false);
        }
        while clicks.last().is_some_and(|c| started.elapsed().as_secs() >= c.0) {
            let (_, x, y) = clicks.pop().unwrap();
            emu.wm.post_mouse(0x0200, x, y); // WM_MOUSEMOVE
            emu.wm.post_mouse(0x0201, x, y); // WM_LBUTTONDOWN
            emu.wm.post_mouse(0x0202, x, y); // WM_LBUTTONUP
        }
        if let Some(d) = dump_after
            && !dumped && started.elapsed() >= d {
                dumped = true;
                emu.dump_threads();
            }
        match (&stop, presenter.as_mut()) {
            (cpu::Stop::StepLimit, None) if emu.steps < max_steps => {}
            // Every thread is waiting for a time: let it pass.
            (cpu::Stop::Sleeping, None) => {
                let ms = emu.idle_ms(8);
                idle += std::time::Duration::from_millis(ms);
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            (cpu::Stop::StepLimit | cpu::Stop::WaitForInput | cpu::Stop::Sleeping, Some(p)) => {
                if !p.pump(&mut emu) {
                    // Host window closed: ask the app to quit, but don't wait forever.
                    if closing_waits == 0 {
                        emu.wm.request_close();
                    }
                    closing_waits += 1;
                    if closing_waits > 200 {
                        break stop;
                    }
                }
                if stop == cpu::Stop::WaitForInput || stop == cpu::Stop::Sleeping {
                    // Sleep until the next thread or timer is due; at most 8 ms, so the window
                    // stays responsive.
                    let ms = emu.idle_ms(8);
                    idle += std::time::Duration::from_millis(ms);
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            _ => break stop,
        }
    };
    if let Some(p) = emu.profile.take() {
        let mut ops: Vec<(u32, u32)> = p.iter().filter(|(k, _)| **k & 0x8000_0000 != 0).map(|(k, v)| (*k & 0xFFFF, *v)).collect();
        ops.sort_by(|a, b| b.1.cmp(&a.1));
        let all: u64 = ops.iter().map(|o| o.1 as u64).sum();
        for (op, n) in ops.iter().take(60) {
            eprintln!("[opcodes] {:5.1}% {}{:02x}", *n as f64 * 100.0 / all as f64, if *op >= 0x100 { "0f " } else { "" }, op & 0xFF);
        }
        let mut rows: Vec<_> = p.into_iter().filter(|(k, _)| *k & 0x8000_0000 == 0).collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u32 = rows.iter().map(|r| r.1).sum();
        let mut by_module: std::collections::HashMap<String, u32> = Default::default();
        for (addr, n) in &rows {
            let loc = emu.win.locate(*addr).unwrap_or_else(|| "?".into());
            *by_module.entry(loc.split('+').next().unwrap_or("?").to_string()).or_insert(0) += n;
        }
        let mut mods: Vec<_> = by_module.into_iter().collect();
        mods.sort_by(|a, b| b.1.cmp(&a.1));
        for (m, n) in mods.iter().take(8) {
            eprintln!("[module] {:5.1}% {m}", *n as f64 * 100.0 / total as f64);
        }
        let top = env::var("XP_PROFILE_TOP").ok().and_then(|v| v.parse().ok()).unwrap_or(45);
        for (addr, n) in rows.iter().take(top) {
            eprintln!("[profile] {:5.1}% {}", *n as f64 * 100.0 / total as f64, emu.win.locate(*addr).unwrap_or_else(|| format!("{addr:#x}")));
        }
    }
    print!("{}", String::from_utf8_lossy(&emu.win.stdout));
    for m in &emu.win.messages {
        eprintln!("[MessageBox] {m}");
    }
    if let Some(out) = screenshot {
        // Same region the live window shows: content only, without the emulated frame.
        crate::d3d::d3d8::finish_present(&mut emu, true);
        let region = emu.wm.content_bounds();
        let desktop = emu.wm.compose();
        let shot = match region {
            Some((x, y, w, h)) => desktop.crop(x, y, w, h),
            None => desktop.crop(0, 0, desktop.width, desktop.height),
        };
        match fs::write(&out, shot.to_ppm()) {
            Ok(()) => eprintln!("screenshot written to {out}"),
            Err(e) => eprintln!("cannot write {out}: {e}"),
        }
    }
    eprintln!("stopped: {stop:?} at eip={:#x} after {} steps, {} frames presented in {:.1}s", emu.eip, emu.steps, emu.win.d3d.frames(), started.elapsed().as_secs_f64());
    // Close devices properly (process::exit skips destructors).
    emu.win.audio.close();
    if matches!(stop, cpu::Stop::Fault(_) | cpu::Stop::InvalidOpcode { .. } | cpu::Stop::DivideError(_)) {
        crash_report(&emu);
    }
    if let cpu::Stop::Exit(code) = stop {
        process::exit(code as i32);
    }
    process::exit(1);
}

/// Where did it go wrong? Registers, the module the crash is in, and return addresses that
/// point into game code (found by scanning the stack), so the spot can be looked up.
fn crash_report(emu: &cpu::Emu) {
    let r = emu.regs;
    eprintln!(
        "[crash] eip={:#x} ({})  eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esp={:#x} ebp={:#x} esi={:#x} edi={:#x}",
        emu.eip,
        emu.win.locate(emu.eip).unwrap_or_else(|| "unknown".into()),
        r[cpu::EAX], r[cpu::ECX], r[cpu::EDX], r[cpu::EBX], r[cpu::ESP], r[cpu::EBP], r[cpu::ESI], r[cpu::EDI]
    );
    let mut shown = 0;
    for i in 0..256u32 {
        let Ok(v) = emu.mem.read(r[cpu::ESP].wrapping_add(4 * i), 4) else { break };
        if let Some(loc) = emu.win.locate(v) {
            eprintln!("[crash]   stack+{:#04x}: {v:#x} = {loc}", 4 * i);
            shown += 1;
            if shown >= 12 {
                break;
            }
        }
    }
}
