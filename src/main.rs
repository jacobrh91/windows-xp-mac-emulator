mod cpu;
mod d3d;
mod game_optimizations;
mod gui;
mod hooks;
mod loader;
mod media;
mod memory;
mod report;
mod scripted;
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
            let data = read(&path);
            if !window {
                run(&path, &data, None, shot);
            }
            // The window lives on the main thread (AppKit wants it there) and the emulator
            // on its own, so a window that stalls (a Space switch, a slow redraw) never holds
            // up the game or the sound its mixer makes. The emulator thread ends the process.
            let band = game_optimizations::top_scroll_band(&path);
            let shared = std::sync::Arc::new(gui::present::Shared::default());
            let link = shared.clone();
            std::thread::Builder::new()
                .name("emulator".into())
                .stack_size(64 << 20)
                .spawn(move || run(&path, &data, Some(link), shot))
                .expect("emulator thread");
            let mut presenter =
                gui::present::Presenter::new("Windows XP emulator", fullscreen, band)
                    .unwrap_or_else(|e| {
                        eprintln!("cannot open window: {e}");
                        process::exit(1);
                    });
            let mut open = true;
            let started = std::time::Instant::now();
            // XP_DEBUG_STALL=<secs>: freeze the window thread for 3 s at that time, as a hidden
            // window does during a Space switch (the game should carry on regardless).
            let mut stall = env::var("XP_DEBUG_STALL")
                .ok()
                .and_then(|v| v.parse::<u64>().ok());
            loop {
                if stall.is_some_and(|t| started.elapsed().as_secs() >= t) {
                    stall = None;
                    eprintln!("[present] window thread stalled for 3 s");
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
                if open && !presenter.pump(&shared) {
                    open = false; // closed: the emulator asks the program to quit
                }
                let wait =
                    std::time::Duration::from_micros(16_667).saturating_sub(presenter.since_pump());
                std::thread::sleep(wait.max(std::time::Duration::from_millis(1)));
            }
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
        println!(
            "  {} ({} functions, {} not yet emulated)",
            i.dll,
            i.functions.len(),
            missing
        );
        if env::var_os("SHOW_MISSING").is_some() {
            for f in i
                .functions
                .iter()
                .filter(|f| !win32::Win32::is_supported(&i.dll, f))
            {
                println!("      missing: {f}");
            }
        }
    }
}

/// Runs the program inside the emulator only; nothing in the image executes natively.
/// `window`: the link to the window on the main thread, if there is one.
fn run(
    path: &str,
    data: &[u8],
    window: Option<std::sync::Arc<gui::present::Shared>>,
    screenshot: Option<String>,
) -> ! {
    let mut emu = cpu::Emu::new();
    if let Some(dir) = std::path::Path::new(path).parent() {
        emu.win.dll_dirs.push(dir.to_path_buf());
        // The guest sees the program's folder as C:\Game, and nothing else on this machine.
        let dir = if dir.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            dir
        };
        emu.win.vfs.mount(r"C:\Game", dir);
        emu.win.vfs.cwd = r"C:\Game".to_string();
        let file = std::path::Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        emu.win.exe_path = format!(r"C:\Game\{file}");
        game_optimizations::setup(&mut emu, &file);
    }
    emu.win.trace = env::var_os("TRACE_API").is_some();
    if env::var_os("XP_SKIP_MOVIES").is_some() {
        emu.win.vfs.hidden_ext.push(".bik".to_string()); // Bink videos: the game carries on without them
    }
    // XP_PROFILE=1 profiles the whole run; XP_PROFILE=<seconds> starts that far in.
    let profile_from: Option<u64> = env::var("XP_PROFILE").ok().map(|v| v.parse().unwrap_or(0));
    emu.mem.watch = env::var("XP_WATCH")
        .ok()
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
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
    let mut last_frame = std::time::Instant::now();
    let mut closing: Option<std::time::Instant> = None;

    // XP_MAX_STEPS stops a run early (with the screenshot) for reproducible checks.
    let max_steps: u64 = env::var("XP_MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000_000);
    let dump_after = env::var("XP_DUMP_AFTER")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs);
    let started = std::time::Instant::now();
    let mut dumped = false;
    let show_fps = env::var_os("XP_FPS").is_some();
    let mut fps_mark = (std::time::Instant::now(), 0u64, 0u64);
    let mut idle = std::time::Duration::ZERO;
    let mut script = scripted::Script::from_env();
    // (real time, clock) at the start of the window the speed-up ratio is measured over.
    let ticks = |e: &cpu::Emu| {
        e.win
            .tick_counter
            .map_or(0, |(at, _)| e.mem.read(at, 4).unwrap_or(0))
    };
    let mut speed_mark = (std::time::Instant::now(), emu.win.now_ms(), ticks(&emu));
    // While speeding up, the clock runs this many times faster than real time (on top of
    // skipping idle waits), so a game that never idles still gets ahead.
    // XP_TURBO=<factor>[:<secs>] sets it, and speeds up the run from that time on (measuring).
    let turbo_env = env::var("XP_TURBO").ok();
    let turbo_arg = |i: usize| {
        turbo_env
            .as_deref()
            .and_then(|v| v.split(':').nth(i)?.parse::<f64>().ok())
    };
    let turbo = turbo_arg(0).unwrap_or(8.0);
    let turbo_from = turbo_env.as_ref().map(|_| turbo_arg(1).unwrap_or(0.0));
    let mut tick = std::time::Instant::now();
    let stop = loop {
        let stop = emu.run(1_000_000);
        let dt = std::mem::replace(&mut tick, std::time::Instant::now()).elapsed();
        if turbo_from.is_some_and(|t| started.elapsed().as_secs_f64() >= t) {
            emu.win.turbo = true;
        }
        if emu.win.turbo {
            emu.win.skew_ms_frac += dt.as_secs_f64() * 1000.0 * (turbo - 1.0);
            let whole = emu.win.skew_ms_frac as u64;
            emu.win.skew_ms += whole;
            emu.win.skew_ms_frac -= whole as f64;
        }
        if let Some(s) = env::var("XP_STOP_AFTER")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            && started.elapsed().as_secs() >= s
        {
            break stop;
        }
        if profile_from.is_some_and(|t| started.elapsed().as_secs() >= t) && emu.profile.is_none() {
            emu.profile = Some(Default::default());
        }
        if show_fps && fps_mark.0.elapsed().as_secs() >= 5 {
            report::fps(&mut emu, started, &mut fps_mark, &mut idle);
        }
        game_optimizations::speedup_tick(&mut emu);
        if d3d::ddraw::show_pending(&mut emu).is_err() {
            eprintln!("[ddraw] cannot read the primary surface");
        }
        script.poll(&mut emu, started);
        let real = speed_mark.0.elapsed();
        if real.as_millis() >= 500 {
            let (clock, game) = (emu.win.now_ms(), ticks(&emu));
            let seconds = real.as_secs_f32();
            emu.win.speedup = match emu.win.tick_counter {
                Some((_, rate)) => game.wrapping_sub(speed_mark.2) as f32 / seconds / rate,
                None => (clock - speed_mark.1) as f32 / 1000.0 / seconds,
            };
            speed_mark = (std::time::Instant::now(), clock, game);
        }
        if let Some(d) = dump_after
            && !dumped
            && started.elapsed() >= d
        {
            dumped = true;
            emu.dump_threads();
        }
        if let Some(link) = &window {
            if link.deliver(&mut emu) && closing.is_none() {
                // Host window closed: ask the app to quit, but don't wait forever.
                emu.wm.request_close();
                closing = Some(std::time::Instant::now());
            }
            if closing.is_some_and(|t| t.elapsed().as_secs() >= 3) {
                break stop;
            }
            // A new picture for the window ~60 times a second.
            if last_frame.elapsed() >= std::time::Duration::from_micros(16_667) {
                last_frame = std::time::Instant::now();
                link.publish(&mut emu);
            }
        }
        match (&stop, &window) {
            (cpu::Stop::StepLimit, None) if emu.steps < max_steps => {}
            (cpu::Stop::StepLimit, Some(_)) => {}
            // Every thread is waiting for a time (or for input): let it pass. At most 8 ms,
            // so pictures and input keep flowing.
            // Speeding up: nobody waits; the clock jumps over the wait instead.
            (cpu::Stop::Sleeping, _) if emu.win.turbo => emu.win.skew_ms += emu.idle_ms(8),
            (cpu::Stop::Sleeping, _) | (cpu::Stop::WaitForInput, Some(_)) => {
                let ms = emu.idle_ms(8);
                idle += std::time::Duration::from_millis(ms);
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            _ => break stop,
        }
    };
    report::profile(&mut emu);
    print!("{}", String::from_utf8_lossy(&emu.win.stdout));
    for m in &emu.win.messages {
        eprintln!("[MessageBox] {m}");
    }
    if let Some(out) = screenshot {
        report::screenshot(&mut emu, &out);
    }
    eprintln!(
        "stopped: {stop:?} at eip={:#x} after {} steps, {} frames presented in {:.1}s",
        emu.eip,
        emu.steps,
        emu.win.d3d.frames() + emu.win.ddraw.frames,
        started.elapsed().as_secs_f64()
    );
    report::registers_and_memory(&emu);
    // Close devices properly (process::exit skips destructors).
    emu.win.audio.close();
    if matches!(
        stop,
        cpu::Stop::Fault(_) | cpu::Stop::InvalidOpcode { .. } | cpu::Stop::DivideError(_)
    ) {
        report::crash(&emu);
    }
    if let cpu::Stop::Exit(code) = stop {
        process::exit(code as i32);
    }
    process::exit(1);
}
