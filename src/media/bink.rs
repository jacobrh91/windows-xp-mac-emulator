//! Bink video, decoded natively on the Mac.
//!
//! Instead of running RAD's `binkw32.dll` (whose x86 decoder is far too slow under the
//! interpreter), the game's calls into it land here. Each movie is decoded by an `ffmpeg`
//! process on the host into raw BGRA frames, and `BinkCopyToBuffer` copies the current frame
//! into the game's buffer (usually a locked Direct3D texture). Only the movie file itself is
//! handed to ffmpeg; it runs as an ordinary Mac program and never touches guest code.
//!
//! A movie's soundtrack is decoded by a second ffmpeg straight to the speakers.
//! `XP_REAL_BINK=1` uses the game's own `binkw32.dll` instead.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;

use super::speaker::SpeakerHandle;

use crate::cpu::{Emu, Res};
use crate::win32::{Args, Handler};

/// Is the native replacement in use (rather than the game's own DLL)?
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XP_REAL_BINK").is_none() && ffmpeg().is_some())
}

/// The host's ffmpeg, if installed. GUI launches may not have Homebrew on PATH.
fn ffmpeg() -> Option<PathBuf> {
    tool("ffmpeg")
}

fn tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

// Offsets in the BINK structure the game reads (bink.h, Bink 1.x).
const OFF_WIDTH: u32 = 0;
const OFF_HEIGHT: u32 = 4;
const OFF_FRAMES: u32 = 8;
const OFF_FRAME_NUM: u32 = 12;
const OFF_LAST_FRAME_NUM: u32 = 16;
const OFF_FRAME_RATE: u32 = 20;
const OFF_FRAME_RATE_DIV: u32 = 24;
const OFF_RECTS: u32 = 52; // 8 x {Left, Top, Width, Height}
const OFF_NUM_RECTS: u32 = 180;
const STRUCT_SIZE: u32 = 0x400;

/// What ffprobe says about a movie's video stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub rate: u32,
    pub div: u32,
}

/// Parse ffprobe's `width,height,num/den,packets` line.
fn parse_probe(line: &str) -> Option<Info> {
    let f: Vec<&str> = line.trim().split(',').collect();
    let (num, den) = f.get(2)?.split_once('/')?;
    Some(Info {
        width: f.first()?.parse().ok()?,
        height: f.get(1)?.parse().ok()?,
        rate: num.parse().ok()?,
        div: den.parse::<u32>().ok()?.max(1),
        frames: f.get(3)?.parse().ok()?,
    })
}

fn probe(path: &Path) -> Option<Info> {
    let out = Command::new(tool("ffprobe")?)
        .args(["-v", "error", "-select_streams", "v:0", "-count_packets", "-show_entries", "stream=width,height,r_frame_rate,nb_read_packets", "-of", "csv=p=0"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

struct Movie {
    info: Info,
    path: PathBuf,
    child: Option<Child>,
    frames: Option<Receiver<Vec<u8>>>,
    /// The frame most recently decoded, BGRA, and its number (1-based; 0 = none yet).
    current: Vec<u8>,
    decoded: u32,
    frame_num: u32,
    /// Emulator time (ms) at which frame 1 was due, adjusted across pauses.
    start_ms: u64,
    paused_at: Option<u64>,
    sound: Option<Soundtrack>,
}

/// A movie's sound, decoded by its own ffmpeg into a speaker voice. Dropping it stops it.
struct Soundtrack {
    child: Child,
    stop: Arc<AtomicBool>,
    speaker: SpeakerHandle,
    voice: u32,
}

impl Soundtrack {
    fn start(path: &Path, speaker: SpeakerHandle, voice: u32) -> Option<Soundtrack> {
        let mut child = Command::new(ffmpeg()?)
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args(["-vn", "-f", "f32le", "-ac", "2", "-ar", &speaker.rate.to_string(), "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, sp) = (stop.clone(), speaker.clone());
        std::thread::spawn(move || {
            // Stay about half a second ahead of the speakers: enough to never run dry, little
            // enough that pausing or closing the movie silences it promptly.
            let ahead = sp.rate as u64 / 2;
            let mut buf = vec![0u8; 8 * 1024];
            loop {
                while !flag.load(Ordering::Relaxed) && {
                    let (queued, played) = sp.progress(voice);
                    queued.saturating_sub(played) > ahead
                } {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if flag.load(Ordering::Relaxed) || out.read_exact(&mut buf).is_err() {
                    return;
                }
                let frames: Vec<[f32; 2]> = buf
                    .chunks_exact(8)
                    .map(|f| [f32::from_le_bytes([f[0], f[1], f[2], f[3]]), f32::from_le_bytes([f[4], f[5], f[6], f[7]])])
                    .collect();
                sp.queue(voice, &frames);
            }
        });
        Some(Soundtrack { child, stop, speaker, voice })
    }
}

impl Drop for Soundtrack {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.speaker.remove(self.voice);
    }
}

impl Movie {
    /// Start (or restart) the decoder. A reader thread keeps a few frames ready.
    fn start_decoder(&mut self) {
        self.stop_decoder();
        let Some(ffmpeg) = ffmpeg() else { return };
        let child = Command::new(ffmpeg)
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(&self.path)
            .args(["-an", "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return };
        let mut out = child.stdout.take().expect("piped stdout");
        let size = (self.info.width * self.info.height * 4) as usize;
        let (tx, rx) = sync_channel::<Vec<u8>>(8);
        std::thread::spawn(move || loop {
            let mut buf = vec![0u8; size];
            if out.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                return;
            }
        });
        self.child = Some(child);
        self.frames = Some(rx);
        self.decoded = 0;
    }

    fn stop_decoder(&mut self) {
        self.frames = None; // the reader thread exits on its next send
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Make `current` hold frame `n`, decoding forward (or restarting for an earlier frame).
    fn decode_to(&mut self, n: u32) {
        if n < self.decoded {
            self.start_decoder();
        }
        while self.decoded < n {
            let Some(rx) = self.frames.as_ref() else { return };
            match rx.recv() {
                Ok(f) => {
                    self.current = f;
                    self.decoded += 1;
                }
                Err(_) => {
                    self.frames = None; // end of stream: keep showing the last frame
                    return;
                }
            }
        }
    }
}

impl Drop for Movie {
    fn drop(&mut self) {
        self.stop_decoder();
    }
}

#[derive(Default)]
pub struct BinkState {
    movies: HashMap<u32, Movie>,
    error: u32,
}

/// Argument bytes / 4 from a decorated stdcall name such as `_BinkOpen@8`.
fn argc_of(name: &str) -> Option<usize> {
    name.rsplit_once('@').and_then(|(_, n)| n.parse::<usize>().ok()).map(|b| b / 4)
}

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "binkw32" {
        return None;
    }
    let argc = argc_of(name)?;
    let base = name.trim_start_matches('_').split('@').next()?;
    let h: Handler = match base {
        "BinkOpen" => bink_open,
        "BinkClose" => |e, a| {
            e.win.bink.movies.remove(&a[0]);
            Ok(0)
        },
        "BinkDoFrame" => |e, a| {
            if let Some(m) = e.win.bink.movies.get_mut(&a[0]) {
                let n = m.frame_num;
                m.decode_to(n);
            }
            Ok(0)
        },
        "BinkNextFrame" => |e, a| {
            if let Some(m) = e.win.bink.movies.get_mut(&a[0]) {
                if m.frame_num < m.info.frames {
                    m.frame_num += 1;
                }
                let n = m.frame_num;
                e.mem.write(a[0] + OFF_LAST_FRAME_NUM, 4, n.saturating_sub(1))?;
                e.mem.write(a[0] + OFF_FRAME_NUM, 4, n)?;
            }
            Ok(0)
        },
        "BinkWait" => |e, a| {
            // 1 = too early for the next frame. Frames are due on the movie's own clock.
            let now = e.win.now_ms();
            let Some(m) = e.win.bink.movies.get(&a[0]) else { return Ok(0) };
            if m.paused_at.is_some() {
                return Ok(1);
            }
            let due = m.start_ms + (m.frame_num.saturating_sub(1) as u64) * 1000 * m.info.div as u64 / m.info.rate.max(1) as u64;
            Ok((now < due) as u32)
        },
        "BinkPause" => |e, a| {
            let now = e.win.now_ms();
            let Some(m) = e.win.bink.movies.get_mut(&a[0]) else { return Ok(0) };
            if let Some(snd) = &m.sound {
                snd.speaker.set_paused(snd.voice, a[1] != 0);
            }
            match (a[1] != 0, m.paused_at) {
                (true, None) => m.paused_at = Some(now),
                (false, Some(t)) => {
                    m.start_ms += now - t;
                    m.paused_at = None;
                }
                _ => {}
            }
            Ok(a[1])
        },
        "BinkGetRects" => |e, a| {
            let Some(info) = e.win.bink.movies.get(&a[0]).map(|m| m.info) else { return Ok(0) };
            for (i, v) in [0, 0, info.width, info.height].iter().enumerate() {
                e.mem.write(a[0] + OFF_RECTS + 4 * i as u32, 4, *v)?;
            }
            e.mem.write(a[0] + OFF_NUM_RECTS, 4, 1)?;
            Ok(1)
        },
        "BinkCopyToBuffer" => copy_to_buffer,
        "BinkGetError" => |e, _| {
            if e.win.bink.error == 0 {
                let p = e.heap_alloc(64)?;
                e.mem.write_bytes(p, b"\0")?;
                e.win.bink.error = p;
            }
            Ok(e.win.bink.error)
        },
        // Sound: accepted and ignored for now.
        "BinkSetSoundSystem" | "BinkSetSoundTrack" | "BinkSetVolume" | "BinkSetSoundOnOff" | "BinkSetPan" => |_, _| Ok(1),
        // The game passes these to BinkSetSoundSystem as "open the sound system" callbacks;
        // they only need to be non-null.
        "BinkOpenDirectSound" | "BinkOpenMiles" | "BinkOpenWaveOut" => |_, _| Ok(1),
        _ => |_, _| Ok(0),
    };
    Some((argc, h))
}

fn bink_open(e: &mut Emu, a: &Args) -> Res<u32> {
    let name = e.mem.read_cstr(a[0])?;
    let Ok(path) = e.win.vfs.resolve(&name) else { return Ok(0) };
    if !path.is_file() {
        return Ok(0);
    }
    let Some(info) = probe(&path) else {
        eprintln!("[bink] cannot read {name}");
        return Ok(0);
    };
    let p = e.heap_alloc(STRUCT_SIZE)?;
    e.mem.write_bytes(p, &vec![0u8; STRUCT_SIZE as usize])?;
    for (off, v) in [
        (OFF_WIDTH, info.width),
        (OFF_HEIGHT, info.height),
        (OFF_FRAMES, info.frames),
        (OFF_FRAME_NUM, 1),
        (OFF_FRAME_RATE, info.rate),
        (OFF_FRAME_RATE_DIV, info.div),
    ] {
        e.mem.write(p + off, 4, v)?;
    }
    let mut m = Movie {
        info,
        path,
        child: None,
        frames: None,
        current: vec![0; (info.width * info.height * 4) as usize],
        decoded: 0,
        frame_num: 1,
        start_ms: e.win.now_ms(),
        paused_at: None,
        sound: None,
    };
    m.start_decoder();
    // The movie's own handle doubles as its speaker voice id.
    m.sound = e.speaker().and_then(|sp| Soundtrack::start(&m.path, sp, p));
    if e.win.trace {
        eprintln!("[bink] {name}: {}x{}, {} frames at {}/{} fps", info.width, info.height, info.frames, info.rate, info.div);
    }
    e.win.bink.movies.insert(p, m);
    Ok(p)
}

/// Convert one BGRA pixel to a Bink surface type: (bytes, bytes per pixel).
fn convert(px: &[u8], kind: u32) -> ([u8; 4], usize) {
    let (b, g, r, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
    let w16 = |v: u32| -> ([u8; 4], usize) { ([v as u8, (v >> 8) as u8, 0, 0], 2) };
    match kind {
        1 => ([b as u8, g as u8, r as u8, 0], 3),                                  // 24: B G R
        2 => ([r as u8, g as u8, b as u8, 0], 3),                                  // 24R
        4 | 6 => ([r as u8, g as u8, b as u8, a as u8], 4),                        // 32R / 32RA
        7 => w16(((a >> 4) << 12) | ((r >> 4) << 8) | ((g >> 4) << 4) | (b >> 4)), // 4444
        8 => w16(((a >> 7) << 15) | ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3)), // 5551
        9 => w16(((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3)),                   // 555
        10 => w16(((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)),                  // 565
        _ => ([b as u8, g as u8, r as u8, a as u8], 4),                            // 32 / 32A: B G R A
    }
}

fn copy_to_buffer(e: &mut Emu, a: &Args) -> Res<u32> {
    // (bink, dest, destpitch, destheight, destx, desty, flags)
    let (dest, pitch, dest_h, dx, dy, kind) = (a[1], a[2], a[3], a[4], a[5], a[6] & 0xF);
    let Some(m) = e.win.bink.movies.get(&a[0]) else { return Ok(0) };
    let (w, h) = (m.info.width, m.info.height);
    let rows = h.min(dest_h.saturating_sub(dy));
    let mut line = Vec::with_capacity((w * 4) as usize);
    for y in 0..rows {
        line.clear();
        let src = &m.current[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
        let bpp = if kind == 0 || kind == 3 || kind == 5 {
            line.extend_from_slice(src); // the decoder's own layout
            4
        } else {
            let mut n = 0;
            for px in src.chunks_exact(4) {
                let (bytes, size) = convert(px, kind);
                line.extend_from_slice(&bytes[..size]);
                n = size;
            }
            n as u32
        };
        e.mem.write_bytes(dest + (dy + y) * pitch + dx * bpp, &line)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decorated_names_give_argument_counts() {
        assert_eq!(argc_of("_BinkOpen@8"), Some(2));
        assert_eq!(argc_of("_BinkCopyToBuffer@28"), Some(7));
        assert_eq!(argc_of("_BinkGetError@0"), Some(0));
        assert!(lookup("binkw32", "_BinkWait@4").is_some_and(|(n, _)| n == 1));
        assert!(lookup("kernel32", "_BinkWait@4").is_none());
    }

    #[test]
    fn probe_output_is_parsed() {
        assert_eq!(parse_probe("640,480,2997/100,606\n"), Some(Info { width: 640, height: 480, rate: 2997, div: 100, frames: 606 }));
        assert_eq!(parse_probe("garbage"), None);
    }

    #[test]
    fn pixel_conversions() {
        let px = [0x10, 0x80, 0xF0, 0xFF]; // B G R A
        assert_eq!(convert(&px, 3), ([0x10, 0x80, 0xF0, 0xFF], 4));
        assert_eq!(convert(&px, 4).0, [0xF0, 0x80, 0x10, 0xFF]);
        assert_eq!(convert(&px, 1), ([0x10, 0x80, 0xF0, 0], 3));
        let (b, n) = convert(&px, 10);
        assert_eq!(n, 2);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), (0xF0 >> 3) << 11 | (0x80 >> 2) << 5 | (0x10 >> 3));
    }

    /// Decode a real movie when the game files and ffmpeg are present.
    #[test]
    fn decodes_the_games_intro_movie() {
        let path = Path::new("exe/ic/app/Movies/relicintro.bik");
        if !path.is_file() || ffmpeg().is_none() {
            return;
        }
        let info = probe(path).expect("ffprobe reads the movie");
        assert_eq!((info.width, info.height), (640, 480));
        assert!(info.frames > 100);
        let mut m = Movie { info, path: path.to_path_buf(), child: None, frames: None, current: vec![], decoded: 0, frame_num: 1, start_ms: 0, paused_at: None, sound: None };
        m.start_decoder();
        m.decode_to(30);
        assert_eq!(m.decoded, 30);
        assert_eq!(m.current.len(), 640 * 480 * 4);
        assert!(m.current.chunks_exact(4).any(|p| p[0] | p[1] | p[2] != 0), "frame 30 is not all black");
    }

    /// The soundtrack decodes into a speaker voice and stays only a little ahead.
    #[test]
    fn the_soundtrack_streams_into_a_voice() {
        let path = Path::new("exe/ic/app/Movies/relicintro.bik");
        if !path.is_file() || ffmpeg().is_none() {
            return;
        }
        let sp = SpeakerHandle::detached(8000);
        let snd = Soundtrack::start(path, sp.clone(), 7).expect("ffmpeg starts");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (queued, _) = sp.progress(7);
        assert!(queued >= 4000, "about half a second is decoded ahead ({queued} frames)");
        assert!(queued <= 4000 + 1024, "and then it waits for the speakers ({queued} frames)");
        let heard = sp.drain(4000);
        assert!(heard.iter().any(|f| f[0] != 0.0), "the intro is not silent");
        drop(snd);
        assert_eq!(sp.progress(7), (0, 0), "closing the movie removes its voice");
    }
}
