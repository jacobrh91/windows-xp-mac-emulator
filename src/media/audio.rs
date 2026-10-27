//! The sound card and the multimedia timers.
//!
//! The waveOut device accepts PCM buffers and plays them against the emulator's clock: a
//! buffer of N bytes at B bytes/second takes N/B seconds, then it is marked done and the
//! program is called back, which is what audio engines (Miles among them) wait for. The
//! samples go to the Mac's speakers (`speaker`), converted to the output's format.
//! Timers from `timeSetEvent` fire from the same pump.

use std::collections::{HashMap, VecDeque};

use super::speaker::{Resampler, Speaker, SpeakerHandle, decode_pcm};
use crate::cpu::{ESP, Emu, Res, Stop};
use crate::win32::threads::Block;
use crate::win32::{Args, Handler};

const MMSYSERR_NOTSUPPORTED: u32 = 8;
const MMSYSERR_INVALHANDLE: u32 = 5;
const MMSYSERR_INVALPARAM: u32 = 11;
const WAVERR_BADFORMAT: u32 = 32;
const WAVERR_STILLPLAYING: u32 = 33;
const WAVERR_UNPREPARED: u32 = 34;

const WHDR_DONE: u32 = 0x1;
const WHDR_PREPARED: u32 = 0x2;
const WHDR_INQUEUE: u32 = 0x10;

const WOM_OPEN: u32 = 0x3BB;
const WOM_CLOSE: u32 = 0x3BC;
const WOM_DONE: u32 = 0x3BD;

const CALLBACK_MASK: u32 = 0x7_0000;
const CALLBACK_WINDOW: u32 = 0x1_0000;
const CALLBACK_THREAD: u32 = 0x2_0000;
const CALLBACK_FUNCTION: u32 = 0x3_0000;
const CALLBACK_EVENT: u32 = 0x5_0000;
const WAVE_FORMAT_QUERY: u32 = 0x10;

const TIME_PERIODIC: u32 = 1;

/// How far past its nominal end a buffer may run, waiting for the speakers, before it is
/// declared done anyway (keeps a stalled audio device from stalling the program).
const SPEAKER_GRACE_MS: u64 = 250;
const TIME_CALLBACK_EVENT_SET: u32 = 0x10;
const TIME_CALLBACK_EVENT_PULSE: u32 = 0x20;

#[derive(Clone, Copy)]
struct Format {
    channels: u32,
    rate: u32,
    bits: u32,
    avg_bytes: u32,
}

struct Queued {
    header: u32,
    len: u32,
    start_ms: u64,
    done_ms: u64,
    /// With real speakers: the buffer is done once they have played up to this frame.
    end_frame: Option<u64>,
}

struct WaveOut {
    format: Format,
    mode: u32,
    callback: u32,
    instance: u32,
    queue: VecDeque<Queued>,
    /// When the last queued buffer finishes (the next one starts then, or now if later).
    tail_ms: u64,
    played_bytes: u64,
    paused_at: Option<u64>,
    volume: u32,
    /// Converts this device's sample rate to the speakers'.
    resampler: Option<Resampler>,
}

struct Timer {
    id: u32,
    delay: u32,
    callback: u32,
    user: u32,
    flags: u32,
    next_ms: u64,
}

#[derive(Default)]
pub struct AudioState {
    waves: HashMap<u32, WaveOut>,
    next_wave: u32,
    timers: Vec<Timer>,
    next_timer: u32,
    /// Callbacks from device events, waiting for the callback thread: (function, args).
    calls: VecDeque<(u32, [u32; 5])>,
    /// The callback thread exists.
    cb_thread: bool,
    /// Its stack pointer between callbacks.
    cb_esp: u32,
    /// The Mac's audio output, opened on first use.
    speaker: Option<Speaker>,
    speaker_tried: bool,
}

impl AudioState {
    /// Stop the speakers (and finish any `XP_SOUND_WAV` recording).
    pub fn close(&mut self) {
        self.speaker = None;
    }
}

impl Emu {
    /// The speakers, if sound is on. Runs on a virtual clock (tests) stay silent.
    pub(crate) fn speaker(&mut self) -> Option<SpeakerHandle> {
        if self.win.virtual_now.is_some() {
            return None;
        }
        let a = &mut self.win.audio;
        if !a.speaker_tried {
            a.speaker_tried = true;
            a.speaker = Speaker::open();
        }
        a.speaker.as_ref().map(|s| s.handle.clone())
    }
}

impl AudioState {
    /// The next time something here is due: a timer, or (with no speakers deciding) the end of
    /// a queued buffer. With speakers, buffers finish as they are heard, so check often.
    pub fn next_due(&self) -> Option<u64> {
        let timers = self.timers.iter().map(|t| t.next_ms).min();
        let buffers = self
            .waves
            .values()
            .filter(|w| w.paused_at.is_none())
            .filter_map(|w| w.queue.front())
            .map(|q| {
                if q.end_frame.is_some() {
                    q.start_ms
                } else {
                    q.done_ms
                }
            })
            .min();
        timers.into_iter().chain(buffers).min()
    }

    /// Anything that needs the pump to run?
    pub fn active(&self) -> bool {
        !self.timers.is_empty() || self.waves.values().any(|w| !w.queue.is_empty())
    }
}

enum Notify {
    Call {
        callback: u32,
        args: [u32; 5],
    },
    SetEvent(u32),
    Post {
        hwnd: u32,
        msg: u32,
        wparam: u32,
        lparam: u32,
    },
}

impl Emu {
    /// Fire due timers and finish audio buffers whose time has come. Returns whether anything
    /// happened (so a blocked scheduler knows to try its threads again).
    pub(crate) fn pump_devices(&mut self) -> Res<bool> {
        if !self.win.audio.active() {
            return Ok(false);
        }
        let now = self.win.now_ms();
        let mut notes: Vec<Notify> = Vec::new();

        // timers
        let mut keep = Vec::new();
        for mut t in std::mem::take(&mut self.win.audio.timers) {
            if t.next_ms > now {
                keep.push(t);
                continue;
            }
            if t.flags & (TIME_CALLBACK_EVENT_SET | TIME_CALLBACK_EVENT_PULSE) != 0 {
                notes.push(Notify::SetEvent(t.callback));
            } else if t.callback != 0 {
                notes.push(Notify::Call {
                    callback: t.callback,
                    args: [t.id, 0, t.user, 0, 0],
                });
            }
            if t.flags & TIME_PERIODIC != 0 {
                // Skip missed periods instead of firing a burst to catch up.
                t.next_ms = now + t.delay.max(1) as u64;
                keep.push(t);
            }
        }
        self.win.audio.timers = keep;

        // audio buffers
        let speaker = self.speaker();
        let mut handles: Vec<u32> = self.win.audio.waves.keys().copied().collect();
        handles.sort();
        for h in handles {
            let played = speaker.as_ref().map(|s| s.progress(h).1);
            let buffering = speaker.as_ref().is_some_and(|s| s.buffering(h));
            loop {
                let Some(w) = self.win.audio.waves.get_mut(&h) else {
                    break;
                };
                // With speakers, a buffer is done when they have played it: their clock, not
                // ours, so the two cannot drift apart. (If they stall, time decides after all.)
                // While they are still building up their prebuffer (a lone short buffer, such as
                // the probe Miles writes at start-up), the emulated clock decides.
                let finished = |q: &Queued| match (q.end_frame, played) {
                    (Some(end), Some(p)) => {
                        p >= end
                            || (buffering && now >= q.done_ms)
                            || now >= q.done_ms + SPEAKER_GRACE_MS
                    }
                    _ => q.done_ms <= now,
                };
                if w.paused_at.is_some() || w.queue.front().is_none_or(|q| !finished(q)) {
                    break;
                }
                let q = w.queue.pop_front().unwrap();
                w.played_bytes += q.len as u64;
                let (mode, callback, instance) = (w.mode, w.callback, w.instance);
                let flags = self.mem.read(q.header + 16, 4)?;
                self.mem
                    .write(q.header + 16, 4, (flags & !WHDR_INQUEUE) | WHDR_DONE)?;
                notes.push(notification(
                    mode, callback, h, WOM_DONE, instance, q.header,
                ));
            }
        }

        let fired = !notes.is_empty();
        for n in notes {
            self.deliver_async(n)?;
        }
        Ok(fired)
    }

    /// A device event (timer, finished buffer): callbacks run on the callback thread, as on
    /// Windows, so one that waits for a lock cannot get tangled up with the code it interrupted.
    fn deliver_async(&mut self, n: Notify) -> Res<()> {
        match n {
            Notify::Call { callback, args } => {
                self.win.audio.calls.push_back((callback, args));
                if !self.win.audio.cb_thread {
                    self.win.audio.cb_thread = true;
                    let stub = self.win.resolve("__emu", "device_callbacks");
                    self.create_thread(stub, 0, 0x10000, false)?;
                }
                Ok(())
            }
            other => self.deliver(other),
        }
    }

    /// Run queued device callbacks on the current thread (tests that have no scheduler).
    #[cfg(test)]
    pub(crate) fn run_device_callbacks_inline(&mut self) -> Res<()> {
        while let Some((callback, args)) = self.win.audio.calls.pop_front() {
            self.deliver(Notify::Call { callback, args })?;
        }
        Ok(())
    }

    /// Callbacks made during an API call (WOM_OPEN, WOM_CLOSE, waveOutReset) run right there.
    fn deliver(&mut self, n: Notify) -> Res<()> {
        match n {
            Notify::Call { callback, args } => {
                // A device callback arrives between any two instructions of whatever was running,
                // like an interrupt, so everything it could disturb is saved and restored. (Real
                // Windows runs these on another thread, which has the same effect.)
                let (regs, eflags, fpu) = (self.regs, self.eflags, self.fpu.clone());
                let result = self.call_function(callback, &args);
                self.regs = regs;
                self.eflags = eflags;
                self.fpu = fpu;
                result?;
            }
            Notify::SetEvent(h) => {
                if let Some(crate::win32::vfs::Obj::Event { signaled, .. }) =
                    self.win.vfs.get_mut(h)
                {
                    *signaled = true;
                }
            }
            Notify::Post {
                hwnd,
                msg,
                wparam,
                lparam,
            } => self.wm.post(hwnd, msg, wparam, lparam),
        }
        Ok(())
    }
}

/// What to do to tell the program about `msg` on wave device `hwo`.
fn notification(
    mode: u32,
    callback: u32,
    hwo: u32,
    msg: u32,
    instance: u32,
    param1: u32,
) -> Notify {
    match mode {
        CALLBACK_FUNCTION => Notify::Call {
            callback,
            args: [hwo, msg, instance, param1, 0],
        },
        CALLBACK_EVENT => Notify::SetEvent(callback),
        CALLBACK_WINDOW => Notify::Post {
            hwnd: callback,
            msg,
            wparam: hwo,
            lparam: param1,
        },
        CALLBACK_THREAD => Notify::Post {
            hwnd: 0,
            msg,
            wparam: hwo,
            lparam: param1,
        },
        _ => Notify::SetEvent(0),
    }
}

fn read_format(e: &Emu, p: u32) -> Res<Option<Format>> {
    let tag = e.mem.read(p, 2)?;
    let (channels, rate, avg, bits) = (
        e.mem.read(p + 2, 2)?,
        e.mem.read(p + 4, 4)?,
        e.mem.read(p + 8, 4)?,
        e.mem.read(p + 14, 2)?,
    );
    let valid = tag == 1
        && (1..=2).contains(&channels)
        && matches!(bits, 8 | 16)
        && rate >= 1000
        && avg > 0;
    Ok(valid.then_some(Format {
        channels,
        rate,
        bits,
        avg_bytes: avg,
    }))
}

fn wave_open(e: &mut Emu, a: &Args) -> Res<u32> {
    let (phwo, pwfx, callback, instance, open) = (a[0], a[2], a[3], a[4], a[5]);
    let Some(format) = read_format(e, pwfx)? else {
        return Ok(WAVERR_BADFORMAT);
    };
    if open & WAVE_FORMAT_QUERY != 0 {
        return Ok(0);
    }
    let handle = 0x7E00_0000 + e.win.audio.next_wave * 4;
    e.win.audio.next_wave += 1;
    let mode = open & CALLBACK_MASK;
    e.win.audio.waves.insert(
        handle,
        WaveOut {
            format,
            mode,
            callback,
            instance,
            queue: VecDeque::new(),
            tail_ms: 0,
            played_bytes: 0,
            paused_at: None,
            volume: 0xFFFF_FFFF,
            resampler: None,
        },
    );
    if phwo != 0 {
        e.mem.write(phwo, 4, handle)?;
    }
    let note = notification(mode, callback, handle, WOM_OPEN, instance, 0);
    if mode != 0 {
        e.deliver(note)?;
    }
    Ok(0)
}

fn wave_close(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(w) = e.win.audio.waves.get(&a[0]) else {
        return Ok(MMSYSERR_INVALHANDLE);
    };
    if !w.queue.is_empty() {
        return Ok(WAVERR_STILLPLAYING);
    }
    let (mode, callback, instance) = (w.mode, w.callback, w.instance);
    e.win.audio.waves.remove(&a[0]);
    if let Some(s) = e.speaker() {
        s.remove(a[0]);
    }
    if mode != 0 {
        e.deliver(notification(mode, callback, a[0], WOM_CLOSE, instance, 0))?;
    }
    Ok(0)
}

fn wave_write(e: &mut Emu, a: &Args) -> Res<u32> {
    let (hwo, hdr) = (a[0], a[1]);
    let now = e.win.now_ms();
    let flags = e.mem.read(hdr + 16, 4)?;
    let len = e.mem.read(hdr + 4, 4)?;
    let Some(w) = e.win.audio.waves.get_mut(&hwo) else {
        return Ok(MMSYSERR_INVALHANDLE);
    };
    if flags & WHDR_PREPARED == 0 {
        return Ok(WAVERR_UNPREPARED);
    }
    let dur = (len as u64 * 1000)
        .div_ceil(w.format.avg_bytes as u64)
        .max(1);
    let start = w.tail_ms.max(now);
    w.tail_ms = start + dur;
    w.queue.push_back(Queued {
        header: hdr,
        len,
        start_ms: start,
        done_ms: start + dur,
        end_frame: None,
    });
    let format = w.format;
    e.mem
        .write(hdr + 16, 4, (flags & !WHDR_DONE) | WHDR_INQUEUE)?;
    // And to the speakers.
    if let Some(s) = e.speaker() {
        let data = e.mem.read(hdr, 4)?;
        if std::env::var_os("XP_SOUND_LOG").is_some() {
            let head: Vec<String> = e
                .mem
                .read_bytes(data, 16.min(len))?
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            eprintln!(
                "[sound] waveOutWrite {hwo:#x} data {data:#x} len {len} heap-tag {:#x} size {:#x} starts {}",
                e.mem.read(data.wrapping_sub(8), 4).unwrap_or(0),
                e.mem.read(data.wrapping_sub(16), 4).unwrap_or(0),
                head.join(" ")
            );
        }
        let pcm = decode_pcm(&e.mem.read_bytes(data, len)?, format.bits, format.channels);
        let mut out = Vec::with_capacity(pcm.len() * 2);
        if let Some(w) = e.win.audio.waves.get_mut(&hwo) {
            w.resampler
                .get_or_insert_with(|| Resampler::new(format.rate, s.rate))
                .process(&pcm, &mut out);
        }
        s.queue(hwo, &out);
        if let Some(q) = e
            .win
            .audio
            .waves
            .get_mut(&hwo)
            .and_then(|w| w.queue.back_mut())
        {
            q.end_frame = Some(s.progress(hwo).0);
        }
    }
    Ok(0)
}

/// Finish every queued buffer right now (waveOutReset).
fn wave_reset(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(w) = e.win.audio.waves.get_mut(&a[0]) else {
        return Ok(MMSYSERR_INVALHANDLE);
    };
    let queued: Vec<Queued> = w.queue.drain(..).collect();
    w.paused_at = None;
    w.tail_ms = 0;
    w.resampler = None;
    let (mode, callback, instance) = (w.mode, w.callback, w.instance);
    if let Some(s) = e.speaker() {
        s.clear(a[0]);
        s.set_paused(a[0], false);
    }
    for q in queued {
        let flags = e.mem.read(q.header + 16, 4)?;
        e.mem
            .write(q.header + 16, 4, (flags & !WHDR_INQUEUE) | WHDR_DONE)?;
        if mode != 0 {
            e.deliver(notification(
                mode, callback, a[0], WOM_DONE, instance, q.header,
            ))?;
        }
    }
    Ok(0)
}

fn wave_pause(e: &mut Emu, a: &Args) -> Res<u32> {
    let now = e.win.now_ms();
    match e.win.audio.waves.get_mut(&a[0]) {
        Some(w) => {
            w.paused_at.get_or_insert(now);
            if let Some(s) = e.speaker() {
                s.set_paused(a[0], true);
            }
            Ok(0)
        }
        None => Ok(MMSYSERR_INVALHANDLE),
    }
}

fn wave_restart(e: &mut Emu, a: &Args) -> Res<u32> {
    let now = e.win.now_ms();
    match e.win.audio.waves.get_mut(&a[0]) {
        Some(w) => {
            if let Some(at) = w.paused_at.take() {
                // Time spent paused does not count as playing.
                let shift = now.saturating_sub(at);
                for q in w.queue.iter_mut() {
                    q.start_ms += shift;
                    q.done_ms += shift;
                }
                w.tail_ms += shift;
            }
            if let Some(s) = e.speaker() {
                s.set_paused(a[0], false);
            }
            Ok(0)
        }
        None => Ok(MMSYSERR_INVALHANDLE),
    }
}

/// How much of the stream has been played, in the unit the caller asked for.
fn wave_position(e: &mut Emu, a: &Args) -> Res<u32> {
    let now = e.win.now_ms();
    let Some(w) = e.win.audio.waves.get(&a[0]) else {
        return Ok(MMSYSERR_INVALHANDLE);
    };
    let mut bytes = w.played_bytes;
    if let (Some(q), None) = (w.queue.front(), w.paused_at)
        && now > q.start_ms
    {
        let (elapsed, total) = (
            (now - q.start_ms).min(q.done_ms - q.start_ms),
            (q.done_ms - q.start_ms).max(1),
        );
        bytes += q.len as u64 * elapsed / total;
    }
    let f = w.format;
    let (kind, value) = match e.mem.read(a[1], 4)? {
        1 => (1, (bytes * 1000 / f.avg_bytes as u64) as u32), // TIME_MS
        2 => (2, (bytes / (f.channels * f.bits / 8).max(1) as u64) as u32), // TIME_SAMPLES
        _ => (4, bytes as u32),                               // TIME_BYTES
    };
    e.mem.write(a[1], 4, kind)?;
    e.mem.write(a[1] + 4, 4, value)?;
    Ok(0)
}

fn time_set_event(e: &mut Emu, a: &Args) -> Res<u32> {
    let (delay, callback, user, flags) = (a[0], a[2], a[3], a[4]);
    let id = e.win.audio.next_timer + 1;
    e.win.audio.next_timer = id;
    let now = e.win.now_ms();
    e.win.audio.timers.push(Timer {
        id,
        delay,
        callback,
        user,
        flags,
        next_ms: now + delay.max(1) as u64,
    });
    Ok(id)
}

/// The callback thread's body: run the next queued callback, then come back here.
fn device_callbacks(e: &mut Emu, _: &Args) -> Res<u32> {
    let stub = e.eip;
    if e.win.audio.cb_esp == 0 {
        e.win.audio.cb_esp = e.regs[ESP];
    }
    let Some((callback, args)) = e.win.audio.calls.pop_front() else {
        return Err(Stop::Blocked(Block::Until(e.win.now_ms() + 2)));
    };
    // A fresh frame each time, so a callback with the wrong calling convention cannot drift the stack.
    e.regs[ESP] = e.win.audio.cb_esp;
    for v in args.iter().rev() {
        e.push(*v)?;
    }
    e.push(stub)?;
    e.eip = callback;
    e.control_transferred = true;
    Ok(0)
}

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll == "__emu" && name == "device_callbacks" {
        return Some((0, device_callbacks));
    }
    if dll != "winmm" {
        return None;
    }
    let r: (usize, Handler) = match name {
        "waveOutGetNumDevs" => (0, |_, _| Ok(1)),
        "waveOutOpen" => (6, wave_open),
        "waveOutClose" => (1, wave_close),
        "waveOutPrepareHeader" => (3, |e, a| {
            let flags = e.mem.read(a[1] + 16, 4)?;
            if !e.win.audio.waves.contains_key(&a[0]) {
                return Ok(MMSYSERR_INVALHANDLE);
            }
            e.mem.write(a[1] + 16, 4, flags | WHDR_PREPARED)?;
            Ok(0)
        }),
        "waveOutUnprepareHeader" => (3, |e, a| {
            let flags = e.mem.read(a[1] + 16, 4)?;
            if flags & WHDR_INQUEUE != 0 {
                return Ok(WAVERR_STILLPLAYING);
            }
            e.mem.write(a[1] + 16, 4, flags & !WHDR_PREPARED)?;
            Ok(0)
        }),
        "waveOutWrite" => (3, wave_write),
        "waveOutReset" => (1, wave_reset),
        "waveOutPause" => (1, wave_pause),
        "waveOutRestart" => (1, wave_restart),
        "waveOutBreakLoop" => (1, |_, _| Ok(0)),
        "waveOutGetPosition" => (3, wave_position),
        "waveOutGetVolume" => (2, |e, a| {
            match e.win.audio.waves.get(&a[0]).map(|w| w.volume) {
                Some(v) => {
                    e.mem.write(a[1], 4, v)?;
                    Ok(0)
                }
                None => Ok(MMSYSERR_INVALHANDLE),
            }
        }),
        "waveOutSetVolume" => (2, |e, a| match e.win.audio.waves.get_mut(&a[0]) {
            Some(w) => {
                w.volume = a[1];
                // Low word is the left channel, high word the right; 0xFFFF is full volume.
                if let Some(s) = e.speaker() {
                    s.set_volume(
                        a[0],
                        (a[1] & 0xFFFF) as f32 / 65535.0,
                        (a[1] >> 16) as f32 / 65535.0,
                    );
                }
                Ok(0)
            }
            None => Ok(MMSYSERR_INVALHANDLE),
        }),
        "waveOutGetID" => (2, |e, a| {
            if e.win.audio.waves.contains_key(&a[0]) {
                e.mem.write(a[1], 4, 0)?;
                Ok(0)
            } else {
                Ok(MMSYSERR_INVALHANDLE)
            }
        }),
        "waveOutMessage" => (4, |_, _| Ok(MMSYSERR_NOTSUPPORTED)),
        "waveOutGetDevCapsA" => (3, |e, a| {
            // WAVEOUTCAPSA: mid, pid, driver version, name[32], formats, channels, reserved, support
            if a[0] != 0 && a[0] != 0xFFFF_FFFF && !e.win.audio.waves.contains_key(&a[0]) {
                return Ok(MMSYSERR_INVALPARAM);
            }
            e.mem.write_bytes(a[1], &[0; 52])?;
            e.mem.write(a[1] + 4, 4, 0x0001_0000)?;
            e.mem.write_bytes(a[1] + 8, b"Silent Audio Device\0")?;
            e.mem.write(a[1] + 40, 4, 0x000F_FFFF)?; // every standard PCM format
            e.mem.write(a[1] + 44, 2, 2)?;
            e.mem.write(a[1] + 48, 4, 0x0000_000F)?; // pitch, playback rate, volume, L/R volume
            Ok(0)
        }),
        "timeSetEvent" => (5, time_set_event),
        "timeKillEvent" => (1, |e, a| {
            let before = e.win.audio.timers.len();
            e.win.audio.timers.retain(|t| t.id != a[0]);
            Ok(if e.win.audio.timers.len() < before {
                0
            } else {
                MMSYSERR_INVALPARAM
            })
        }),
        "timeGetDevCaps" => (2, |e, a| {
            e.mem.write(a[0], 4, 1)?; // wPeriodMin
            e.mem.write(a[0] + 4, 4, 1_000_000)?; // wPeriodMax
            Ok(0)
        }),
        _ => return None,
    };
    Some(r)
}

#[cfg(test)]
mod tests;
