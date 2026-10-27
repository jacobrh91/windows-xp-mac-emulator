//! The Mac's speakers: a small mixer feeding CoreAudio (through `cpal`).
//!
//! Each sound source the guest opens (a waveOut device, a movie's soundtrack) is a *voice*:
//! a queue of stereo frames already converted to the output's sample rate. The audio
//! callback, on CoreAudio's own thread, sums the voices. Producers only ever append, so the
//! emulator decides *what* plays and the hardware decides *when*.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// More than this much queued audio (seconds) means the guest is running ahead of the
/// speakers; the oldest frames are dropped to keep latency bounded.
const MAX_QUEUED_SECS: f32 = 1.0;

/// A voice starts (and restarts after running dry) only once this much is queued, so that
/// the emulator's timing jitter does not reach the speakers as clicks.
const PREBUFFER_SECS: f32 = 0.08;

#[derive(Default)]
struct Voice {
    frames: VecDeque<[f32; 2]>,
    paused: bool,
    volume: [f32; 2],
    /// Enough is queued to play; cleared when the queue runs dry.
    primed: bool,
    /// Output frames spent waiting to prime with something queued: a short sound (or the
    /// tail of one) plays after one prebuffer period even if no more arrives.
    waited: usize,
    /// Frames ever queued, and frames already sent to the speakers (or discarded): the
    /// difference is what is still waiting.
    total_queued: u64,
    played: u64,
}

/// `XP_SOUND_LOG=1`: every block queued to a voice (with its peak level) and every removal.
fn sound_log() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XP_SOUND_LOG").is_some())
}

#[derive(Default)]
struct Mixer {
    voices: HashMap<u32, Voice>,
    prebuffer: usize,
}

impl Mixer {
    /// The next output frame: every playing voice, summed.
    fn next_frame(&mut self) -> [f32; 2] {
        let mut out = [0.0f32; 2];
        for v in self.voices.values_mut() {
            if v.paused {
                continue;
            }
            if !v.primed {
                if v.frames.len() < self.prebuffer
                    && (v.frames.is_empty() || v.waited < self.prebuffer)
                {
                    if !v.frames.is_empty() {
                        v.waited += 1;
                    }
                    continue;
                }
                v.primed = true;
                v.waited = 0;
            }
            match v.frames.pop_front() {
                Some(f) => {
                    v.played += 1;
                    out[0] += f[0] * v.volume[0];
                    out[1] += f[1] * v.volume[1];
                }
                None => v.primed = false,
            }
        }
        [out[0].clamp(-1.0, 1.0), out[1].clamp(-1.0, 1.0)]
    }
}

/// A cheap, thread-safe way to queue sound; the video decoder thread holds one too.
/// The master volume in percent, changed from the emulator's menu while the sound plays.
pub static MASTER_PERCENT: AtomicU32 = AtomicU32::new(100);

#[derive(Clone)]
pub struct SpeakerHandle {
    mixer: Arc<Mutex<Mixer>>,
    pub rate: u32,
}

impl SpeakerHandle {
    /// A mixer with no device behind it (tests read its output with `drain`).
    #[cfg(test)]
    pub fn detached(rate: u32) -> SpeakerHandle {
        SpeakerHandle::new(rate, 0)
    }

    fn new(rate: u32, prebuffer: usize) -> SpeakerHandle {
        SpeakerHandle {
            mixer: Arc::new(Mutex::new(Mixer {
                voices: HashMap::new(),
                prebuffer,
            })),
            rate,
        }
    }

    fn with_voice<R>(&self, id: u32, f: impl FnOnce(&mut Voice) -> R) -> R {
        let mut m = self.mixer.lock().unwrap();
        let v = m.voices.entry(id).or_insert_with(|| Voice {
            volume: [1.0, 1.0],
            ..Voice::default()
        });
        f(v)
    }

    /// Append frames (stereo, at `self.rate`) to voice `id`.
    pub fn queue(&self, id: u32, frames: &[[f32; 2]]) {
        if sound_log() {
            let peak = frames.iter().flatten().fold(0.0f32, |m, &x| m.max(x.abs()));
            eprintln!(
                "[sound] {:?} voice {id:#x} +{} frames, peak {peak:.3}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
                    % 1000.0,
                frames.len()
            );
        }
        let cap = (self.rate as f32 * MAX_QUEUED_SECS) as usize;
        self.with_voice(id, |v| {
            v.frames.extend(frames.iter().copied());
            v.total_queued += frames.len() as u64;
            let excess = v.frames.len().saturating_sub(cap);
            v.frames.drain(..excess);
            v.played += excess as u64;
        })
    }

    /// (frames ever queued, frames played) for voice `id`. A producer that notes the first
    /// number after queueing something knows it has been heard once the second reaches it.
    pub fn progress(&self, id: u32) -> (u64, u64) {
        self.mixer
            .lock()
            .unwrap()
            .voices
            .get(&id)
            .map_or((0, 0), |v| (v.total_queued, v.played))
    }

    /// Frames of voice `id` not yet played.
    #[cfg(test)]
    pub fn queued(&self, id: u32) -> usize {
        self.mixer
            .lock()
            .unwrap()
            .voices
            .get(&id)
            .map_or(0, |v| v.frames.len())
    }

    /// Is voice `id` holding back audio while it builds up its prebuffer?
    pub fn buffering(&self, id: u32) -> bool {
        self.mixer
            .lock()
            .unwrap()
            .voices
            .get(&id)
            .is_some_and(|v| !v.primed && !v.frames.is_empty())
    }

    /// Drop everything queued (it counts as played).
    pub fn clear(&self, id: u32) {
        self.with_voice(id, |v| {
            v.played += v.frames.len() as u64;
            v.frames.clear();
        });
    }

    pub fn set_paused(&self, id: u32, paused: bool) {
        self.with_voice(id, |v| v.paused = paused);
    }

    pub fn set_volume(&self, id: u32, left: f32, right: f32) {
        self.with_voice(id, |v| v.volume = [left, right]);
    }

    pub fn remove(&self, id: u32) {
        if sound_log() {
            eprintln!("[sound] voice {id:#x} removed");
        }
        self.mixer.lock().unwrap().voices.remove(&id);
    }

    /// Mix `n` frames as the audio callback would (tests).
    #[cfg(test)]
    pub fn drain(&self, n: usize) -> Vec<[f32; 2]> {
        let mut m = self.mixer.lock().unwrap();
        (0..n).map(|_| m.next_frame()).collect()
    }
}

/// The open output stream. Dropping it stops the sound.
pub struct Speaker {
    _stream: cpal::Stream,
    pub handle: SpeakerHandle,
    /// `XP_SOUND_WAV=<file>`: everything played (before the master volume), saved as a WAV file
    /// at the end (debugging).
    recording: Option<(String, Arc<Mutex<Vec<[f32; 2]>>>)>,
}

impl Drop for Speaker {
    fn drop(&mut self) {
        if let Some((path, frames)) = &self.recording {
            let frames = frames.lock().unwrap();
            match std::fs::write(path, wav_bytes(&frames, self.handle.rate)) {
                Ok(()) => eprintln!(
                    "[sound] {:.1} s of output written to {path}",
                    frames.len() as f32 / self.handle.rate as f32
                ),
                Err(e) => eprintln!("[sound] cannot write {path}: {e}"),
            }
        }
    }
}

/// 16-bit stereo WAV file contents.
fn wav_bytes(frames: &[[f32; 2]], rate: u32) -> Vec<u8> {
    let data_len = frames.len() as u32 * 4;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [16u32, 0x0002_0001, rate, rate * 4, 0x0010_0004] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for f in frames {
        for c in f {
            b.extend_from_slice(&((c.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
    }
    b
}

impl Speaker {
    /// Open the default output device. None if there is none, it does not take 32-bit float
    /// samples, or `XP_MUTE` is set.
    pub fn open() -> Option<Speaker> {
        if std::env::var_os("XP_MUTE").is_some() {
            return None;
        }
        let device = cpal::default_host().default_output_device()?;
        let supported = device.default_output_config().ok()?;
        if supported.sample_format() != cpal::SampleFormat::F32 {
            eprintln!("[sound] output device does not take float samples; staying silent");
            return None;
        }
        let config = supported.config();
        let channels = config.channels as usize;
        // XP_VOLUME=<percent>: master volume (default 100).
        let master = std::env::var("XP_VOLUME")
            .ok()
            .and_then(|v| v.trim_end_matches('%').parse::<f32>().ok())
            .map_or(1.0, |p| (p / 100.0).clamp(0.0, 1.0));
        MASTER_PERCENT.store((master * 100.0).round() as u32, Ordering::Relaxed);
        let handle = SpeakerHandle::new(
            config.sample_rate,
            (config.sample_rate as f32 * PREBUFFER_SECS) as usize,
        );
        let mixer = handle.mixer.clone();
        let recording = std::env::var("XP_SOUND_WAV")
            .ok()
            .map(|p| (p, Arc::new(Mutex::new(Vec::new()))));
        let tape = recording.as_ref().map(|r| r.1.clone());
        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let master = MASTER_PERCENT.load(Ordering::Relaxed) as f32 / 100.0;
                    let mut m = mixer.lock().unwrap();
                    for frame in out.chunks_mut(channels) {
                        let [l, r] = m.next_frame();
                        // The recording is of the mix itself, before the volume, so a silent
                        // run (XP_VOLUME=0) still records everything.
                        if let Some(t) = &tape {
                            t.lock().unwrap().push([l, r]);
                        }
                        let (l, r) = (l * master, r * master);
                        match frame.len() {
                            1 => frame[0] = (l + r) * 0.5,
                            _ => {
                                frame[0] = l;
                                frame[1] = r;
                                frame[2..].fill(0.0);
                            }
                        }
                    }
                },
                |err| eprintln!("[sound] {err}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        eprintln!(
            "[sound] playing through the default output device at {} Hz, volume {:.0}%",
            handle.rate,
            master * 100.0
        );
        Some(Speaker {
            _stream: stream,
            handle,
            recording,
        })
    }
}

/// Converts a stream of stereo frames from one sample rate to another (linear interpolation),
/// carrying its position across calls so consecutive buffers join without clicks.
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Resampler {
        Resampler {
            step: from_rate as f64 / to_rate.max(1) as f64,
            pos: 0.0,
            prev: [0.0; 2],
        }
    }

    pub fn process(&mut self, input: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        let n = input.len();
        if n == 0 {
            return;
        }
        // Index 0 is the last frame of the previous call, 1..=n are this call's.
        let at = |i: usize| if i == 0 { self.prev } else { input[i - 1] };
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = input[n - 1];
    }
}

/// Decode PCM bytes (8-bit unsigned or 16-bit signed, mono or stereo) to stereo floats.
pub fn decode_pcm(bytes: &[u8], bits: u32, channels: u32) -> Vec<[f32; 2]> {
    let sample = |b: &[u8]| -> f32 {
        if bits == 8 {
            (b[0] as f32 - 128.0) / 128.0
        } else {
            i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0
        }
    };
    let width = (bits / 8) as usize;
    let frame = width * channels as usize;
    bytes
        .chunks_exact(frame)
        .map(|f| {
            let l = sample(&f[..width]);
            let r = if channels == 2 {
                sample(&f[width..])
            } else {
                l
            };
            [l, r]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_decoding() {
        assert_eq!(
            decode_pcm(&[128, 255, 0], 8, 1),
            vec![[0.0, 0.0], [127.0 / 128.0; 2], [-1.0, -1.0]]
        );
        let s16: Vec<u8> = [16384i16, -32768]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(decode_pcm(&s16, 16, 2), vec![[0.5, -1.0]]);
    }

    #[test]
    fn resampling_keeps_duration_and_joins_buffers_smoothly() {
        // 22050 -> 44100 doubles the frame count, whether fed at once or in pieces.
        let ramp: Vec<[f32; 2]> = (0..100).map(|i| [i as f32 / 100.0; 2]).collect();
        let mut whole = Vec::new();
        Resampler::new(22050, 44100).process(&ramp, &mut whole);
        let mut pieces = Vec::new();
        let mut r = Resampler::new(22050, 44100);
        for chunk in ramp.chunks(7) {
            r.process(chunk, &mut pieces);
        }
        assert_eq!(whole.len(), 200);
        assert_eq!(whole, pieces, "buffer boundaries make no difference");
        assert!(
            whole.windows(2).all(|w| w[1][0] >= w[0][0]),
            "a ramp stays a ramp"
        );
        let mut down = Vec::new();
        Resampler::new(48000, 24000).process(&ramp, &mut down);
        assert_eq!(down.len(), 50);
    }

    #[test]
    fn the_mixer_sums_voices_and_honours_pause_and_volume() {
        let s = SpeakerHandle::detached(100);
        s.queue(1, &[[0.25, 0.25]; 4]);
        s.queue(2, &[[0.5, -0.5]; 2]);
        assert_eq!(s.drain(1), vec![[0.75, -0.25]]);
        s.set_paused(2, true);
        assert_eq!(s.drain(1), vec![[0.25, 0.25]], "paused voices wait");
        s.set_volume(1, 0.0, 1.0);
        assert_eq!(s.drain(1), vec![[0.0, 0.25]]);
        s.set_paused(2, false);
        s.clear(1);
        assert_eq!(
            s.drain(1),
            vec![[0.5, -0.5]],
            "voice 2's second frame, kept while paused"
        );
        assert_eq!(
            s.drain(1),
            vec![[0.0, 0.0]],
            "silence when nothing is queued"
        );
        assert_eq!(s.queued(2), 0);
    }

    #[test]
    fn a_voice_waits_for_its_prebuffer_and_again_after_running_dry() {
        let s = SpeakerHandle::new(100, 3);
        s.queue(1, &[[0.5, 0.5]; 2]);
        assert_eq!(
            s.drain(1),
            vec![[0.0, 0.0]],
            "only 2 of 3 frames queued: wait"
        );
        s.queue(1, &[[0.5, 0.5]; 1]);
        assert_eq!(
            s.drain(4),
            vec![[0.5, 0.5], [0.5, 0.5], [0.5, 0.5], [0.0, 0.0]]
        );
        s.queue(1, &[[0.25, 0.25]; 1]);
        assert_eq!(
            s.drain(1),
            vec![[0.0, 0.0]],
            "ran dry, so it buffers up again"
        );
        assert!(s.buffering(1));
        assert_eq!(
            s.drain(3),
            vec![[0.0, 0.0], [0.0, 0.0], [0.25, 0.25]],
            "but a lone short sound still plays after one prebuffer period"
        );
    }

    #[test]
    fn progress_counts_what_was_heard() {
        let s = SpeakerHandle::detached(100);
        s.queue(1, &[[0.1, 0.1]; 5]);
        assert_eq!(s.progress(1), (5, 0));
        s.drain(3);
        assert_eq!(s.progress(1), (5, 3));
        s.clear(1);
        assert_eq!(s.progress(1), (5, 5), "cleared frames count as played");
    }

    #[test]
    fn a_runaway_producer_is_capped() {
        let s = SpeakerHandle::detached(100);
        s.queue(1, &[[0.1, 0.1]; 500]);
        assert_eq!(s.queued(1), 100, "at most a second of audio waits");
    }
}
