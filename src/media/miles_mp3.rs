//! Miles' MP3 decoder (`mssmp3.asi`), done natively.
//!
//! Miles plays MP3 music through an "ASI" codec plugin. The plugin stays loaded and registers
//! itself with Miles as usual, but its stream functions are patched (see `hooks`) to land
//! here, where `nanomp3-core` (a bit-exact Rust port of minimp3) does the decoding. Under the
//! interpreter the original decoder took about a sixth of all the game's instructions.
//!
//! The functions follow the plugin's own behaviour (all stdcall):
//! - `ASI_stream_open(user, fetch, total_size)` -> stream, or 0 if the data is not MP3.
//!   Compressed bytes come from the guest's `fetch(user, dest, bytes, offset)` callback;
//!   offset -1 means "carry on from where you are".
//! - `ASI_stream_process(stream, buffer, bytes)` -> bytes of 16-bit PCM produced; the rest of
//!   the buffer is zeroed once the stream runs out.
//! - `ASI_stream_seek(stream, offset)`: restart decoding from a byte offset (-2: just reset).
//! - `ASI_stream_attribute(stream, token)` and `ASI_stream_set_preference(stream, token, *v)`.
//! - `ASI_stream_close(stream)`.
//!
//! `XP_MP3_LOG=1` reports streams as they open; `XP_MP3_LOG=2` also every fetch, process call,
//! attribute query and close, with timestamps.

use std::collections::HashMap;

use nanomp3_core::{DecodeError, Decoder, MAX_SAMPLES_PER_FRAME};

use crate::cpu::{Emu, Res};
use crate::win32::Args;

/// The most compressed data asked for at once (the plugin's "Minimum input block size").
const FETCH_BYTES: u32 = 4096;
/// For free-format streams, whose frame length the header does not give: read this far ahead.
const INPUT_AHEAD: usize = FETCH_BYTES as usize;

// Attribute tokens, as the plugin registers them.
const INPUT_BIT_RATE: u32 = 4;
const INPUT_SAMPLE_RATE: u32 = 5;
const INPUT_SAMPLE_WIDTH: u32 = 6;
const INPUT_CHANNELS: u32 = 7;
const OUTPUT_BIT_RATE: u32 = 8;
const OUTPUT_SAMPLE_RATE: u32 = 9;
const OUTPUT_SAMPLE_WIDTH: u32 = 10;
const OUTPUT_CHANNELS: u32 = 11;
const POSITION: u32 = 12;
const PERCENT_DONE: u32 = 13;
const MIN_INPUT_BLOCK: u32 = 14;
const MPEG_VERSION: u32 = 15;
const MPEG_LAYER: u32 = 16;
/// Preferences 17..=19 are stored and read back.
const FIRST_PREFERENCE: u32 = 17;

/// The length in bytes of the MPEG audio frame whose header is `h`: 0 for free format, None if
/// `h` is not a frame header.
fn frame_length(h: &[u8]) -> Option<usize> {
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = h[1] >> 3 & 3; // 3: MPEG-1, 2: MPEG-2, 0: MPEG-2.5
    let layer = h[1] >> 1 & 3; // 3: I, 2: II, 1: III
    let (br_index, sr_index, pad) = (
        (h[2] >> 4) as usize,
        (h[2] >> 2 & 3) as usize,
        (h[2] >> 1 & 1) as usize,
    );
    if version == 1 || layer == 0 || br_index == 15 || sr_index == 3 {
        return None;
    }
    if br_index == 0 {
        return Some(0);
    }
    const KBPS: [[u16; 15]; 5] = [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ], // MPEG-1 I
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ], // MPEG-1 II
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ], // MPEG-1 III
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ], // MPEG-2 I
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160], // MPEG-2 II, III
    ];
    let mpeg1 = version == 3;
    let table = match (mpeg1, layer) {
        (true, 3) => 0,
        (true, 2) => 1,
        (true, _) => 2,
        (false, 3) => 3,
        (false, _) => 4,
    };
    let bitrate = KBPS[table][br_index] as usize * 1000;
    let rate = [44100, 48000, 32000][sr_index]
        >> match version {
            3 => 0,
            2 => 1,
            _ => 2,
        };
    Some(match layer {
        3 => (12 * bitrate / rate + pad) * 4,
        1 if !mpeg1 => 72 * bitrate / rate + pad,
        _ => 144 * bitrate / rate + pad,
    })
}

pub struct Mp3Stream {
    user: u32,
    fetch: u32,
    total: u32,
    /// Guest buffer the fetch callback writes into.
    scratch: u32,
    dec: Box<Decoder>,
    /// Compressed bytes not yet decoded: `input[used..]`.
    input: Vec<u8>,
    used: usize,
    /// The fetch callback had nothing more (for now: retried on the next process call).
    eof: bool,
    /// Offset for the next fetch (-1: continue).
    next_offset: i32,
    /// Compressed bytes received so far.
    position: u32,
    /// Decoded PCM (little-endian 16-bit, interleaved) waiting to be handed out.
    pcm: Vec<u8>,
    pcm_used: usize,
    rate: u32,
    channels: u32,
    /// Bits per second.
    bitrate: u32,
    layer: u32,
    prefs: [u32; 3],
    /// XP_MP3_DUMP: every compressed byte fetched, written out when the stream closes.
    dump: Option<Vec<u8>>,
}

impl Mp3Stream {
    /// Ask the guest for up to `bytes` more compressed bytes. Returns false at the end of the
    /// stream.
    fn fetch_more(&mut self, e: &mut Emu, bytes: u32) -> Res<bool> {
        let got = e.call_function(
            self.fetch,
            &[
                self.user,
                self.scratch,
                bytes.clamp(1, FETCH_BYTES),
                self.next_offset as u32,
            ],
        )? as i32;
        self.next_offset = -1;
        if std::env::var_os("XP_MP3_LOG").is_some_and(|v| v == "2") {
            eprintln!(
                "[mp3] {:.3} fetch for user {:#x} -> {got} (position {} of {})",
                e.win.elapsed_ms() as f64 / 1000.0,
                self.user,
                self.position,
                self.total
            );
        }
        if got <= 0 {
            self.eof = true;
            return Ok(false);
        }
        let got = (got as u32).min(FETCH_BYTES);
        let bytes = e.mem.read_bytes(self.scratch, got)?;
        if let Some(d) = self.dump.as_mut() {
            d.extend_from_slice(&bytes);
        }
        // Drop what has been decoded before appending, so the buffer stays small.
        self.input.drain(..self.used);
        self.used = 0;
        self.input.extend_from_slice(&bytes);
        self.position = self.position.wrapping_add(got);
        Ok(true)
    }

    /// Compressed bytes buffered and not yet decoded.
    fn buffered(&self) -> usize {
        self.input.len() - self.used
    }

    /// Have at least `n` bytes buffered, unless the data runs out first. False if it did.
    fn fill(&mut self, e: &mut Emu, n: usize) -> Res<bool> {
        while self.buffered() < n {
            if self.eof || !self.fetch_more(e, (n - self.buffered()) as u32)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Bring the next frame in, and no more than the header of the one after (the decoder
    /// accepts a frame once it sees the next one). Miles ends a stream once its file has been
    /// read to the end, so reading further ahead than this cut off the end of every sound:
    /// half a second of each speech line.
    fn fill_frame(&mut self, e: &mut Emu) -> Res<()> {
        loop {
            if !self.fill(e, 4)? {
                return Ok(());
            }
            let h = &self.input[self.used..self.used + 4];
            if h[..3] == *b"ID3" {
                // An ID3v2 tag: skip it (10-byte header, then a "synchsafe" size).
                if !self.fill(e, 10)? {
                    return Ok(());
                }
                let t = &self.input[self.used..self.used + 10];
                let size = t[6..10]
                    .iter()
                    .fold(0usize, |a, &b| a << 7 | (b & 0x7F) as usize);
                let mut skip = 10 + size + if t[5] & 0x10 != 0 { 10 } else { 0 };
                while skip > 0 {
                    if self.buffered() == 0 && !self.fill(e, skip.min(FETCH_BYTES as usize))? {
                        return Ok(());
                    }
                    let n = skip.min(self.buffered());
                    self.used += n;
                    skip -= n;
                }
                continue;
            }
            match frame_length(h) {
                Some(0) => {
                    self.fill(e, INPUT_AHEAD)?;
                    return Ok(());
                }
                Some(len) => {
                    self.fill(e, len + 4)?;
                    return Ok(());
                }
                // Not a frame header: skip a byte and look again.
                None => self.used += 1,
            }
        }
    }

    /// Decode one more frame into `pcm`. Returns false when nothing more will come.
    fn decode_frame(&mut self, e: &mut Emu) -> Res<bool> {
        loop {
            self.fill_frame(e)?;
            if self.used >= self.input.len() {
                return Ok(false);
            }
            let mut out = [0i16; MAX_SAMPLES_PER_FRAME];
            let (consumed, result) = self.dec.decode(&self.input[self.used..], &mut out);
            self.used += consumed;
            match result {
                Ok(info) => {
                    self.rate = info.sample_rate;
                    self.channels = info.channels.num() as u32;
                    self.bitrate = info.bitrate * 1000;
                    self.layer = info.layer as u32;
                    let n = info.samples_produced * info.channels.num() as usize;
                    self.pcm.drain(..self.pcm_used);
                    self.pcm_used = 0;
                    self.pcm
                        .extend(out[..n].iter().flat_map(|s| s.to_le_bytes()));
                    return Ok(true);
                }
                Err(DecodeError::NoFrame) => {
                    if self.eof && self.used >= self.input.len() {
                        return Ok(false);
                    }
                    if consumed == 0 {
                        return Ok(false); // nothing usable and nothing consumed: stop rather than spin
                    }
                }
                // Skipped (start-up after a seek, a damaged frame): keep going.
                Err(_) => {}
            }
        }
    }

    /// How far into the compressed data decoding has got (the "Position" attribute). Miles
    /// judges from this when a stream is over, so it must not count input that is only
    /// buffered: reporting the read-ahead cut the end off every speech line.
    fn decoded_position(&self) -> u32 {
        self.position
            .saturating_sub((self.input.len() - self.used) as u32)
    }

    fn reset_decoder(&mut self) {
        *self.dec = Decoder::new();
        self.pcm.clear();
        self.pcm_used = 0;
    }
}

#[derive(Default)]
pub struct Mp3State {
    streams: HashMap<u32, Mp3Stream>,
}

/// Take a stream out of the table while a handler works on it (it calls back into the guest).
fn with_stream<R>(
    e: &mut Emu,
    h: u32,
    f: impl FnOnce(&mut Emu, &mut Mp3Stream) -> Res<R>,
) -> Res<Option<R>> {
    let Some(mut s) = e.win.mp3.streams.remove(&h) else {
        return Ok(None);
    };
    let r = f(e, &mut s);
    e.win.mp3.streams.insert(h, s);
    r.map(Some)
}

pub fn stream_open(e: &mut Emu, a: &Args) -> Res<u32> {
    let handle = e.heap_alloc(16)?;
    let scratch = e.heap_alloc(FETCH_BYTES)?;
    let mut s = Mp3Stream {
        user: a[0],
        fetch: a[1],
        total: a[2],
        scratch,
        dec: Box::new(Decoder::new()),
        input: Vec::new(),
        used: 0,
        eof: false,
        next_offset: -1,
        position: 0,
        pcm: Vec::new(),
        pcm_used: 0,
        rate: 0,
        channels: 0,
        bitrate: 0,
        layer: 3,
        prefs: [u32::MAX; 3],
        dump: std::env::var_os("XP_MP3_DUMP").map(|_| Vec::new()),
    };
    // Like the original, read the first frame now: Miles asks for the format straight away,
    // and data that is not MP3 is refused here.
    if !s.decode_frame(e)? {
        if std::env::var_os("XP_MP3_LOG").is_some() {
            eprintln!(
                "[mp3] stream refused: no MPEG audio in {} bytes fetched",
                s.position
            );
        }
        e.heap_free(scratch);
        e.heap_free(handle);
        return Ok(0);
    }
    if std::env::var_os("XP_MP3_LOG").is_some() {
        eprintln!(
            "[mp3] stream {handle:#x} opened (user {:#x}, {} bytes): {} Hz, {} channels, {} kbit/s",
            s.user,
            s.total,
            s.rate,
            s.channels,
            s.bitrate / 1000
        );
    }
    e.win.mp3.streams.insert(handle, s);
    Ok(handle)
}

pub fn stream_process(e: &mut Emu, a: &Args) -> Res<u32> {
    let (h, buffer, want) = (a[0], a[1], a[2] as i32);
    if want <= 0 {
        return Ok(0);
    }
    let want = want as usize;
    let made = with_stream(e, h, |e, s| {
        // Streamed files arrive in the background: "no data" from the fetch callback means
        // "not yet", so every call tries again (as the original does).
        s.eof = false;
        while s.pcm.len() - s.pcm_used < want {
            if !s.decode_frame(e)? {
                break;
            }
        }
        let n = (s.pcm.len() - s.pcm_used).min(want);
        e.mem
            .write_bytes(buffer, &s.pcm[s.pcm_used..s.pcm_used + n])?;
        s.pcm_used += n;
        if std::env::var_os("XP_MP3_LOG").is_some_and(|v| v == "2") {
            eprintln!(
                "[mp3] {:.3} {h:#x} want {want} made {n} input {} eof {}",
                e.win.elapsed_ms() as f64 / 1000.0,
                s.input.len() - s.used,
                s.eof
            );
        }
        if n < want {
            // Out of data: silence for the rest, as the original does.
            e.mem.write_bytes(buffer + n as u32, &vec![0; want - n])?;
        }
        Ok(n as u32)
    })?;
    Ok(made.unwrap_or(0))
}

pub fn stream_seek(e: &mut Emu, a: &Args) -> Res<u32> {
    let (h, offset) = (a[0], a[1] as i32);
    if let Some(s) = e.win.mp3.streams.get_mut(&h) {
        s.reset_decoder();
        if offset != -2 {
            // A real seek: buffered input belongs to the old position.
            s.input.clear();
            s.used = 0;
            s.eof = false;
            s.next_offset = offset;
            if offset >= 0 {
                s.position = offset as u32;
            }
        }
    }
    Ok(0)
}

pub fn stream_close(e: &mut Emu, a: &Args) -> Res<u32> {
    if std::env::var_os("XP_MP3_LOG").is_some_and(|v| v == "2") {
        eprintln!(
            "[mp3] {:.3} {:#x} closed",
            e.win.elapsed_ms() as f64 / 1000.0,
            a[0]
        );
    }
    if let Some(s) = e.win.mp3.streams.remove(&a[0]) {
        // XP_MP3_DUMP=<dir>: the stream's compressed data, as <dir>/<seconds>-<stream>.mp3.
        if let (Some(d), Some(dir)) = (&s.dump, std::env::var_os("XP_MP3_DUMP")) {
            let name = format!("{:07.3}-{:x}.mp3", e.win.elapsed_ms() as f64 / 1000.0, a[0]);
            let _ = std::fs::write(std::path::Path::new(&dir).join(name), d);
        }
        e.heap_free(s.scratch);
        e.heap_free(a[0]);
    }
    Ok(0)
}

pub fn stream_attribute(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(s) = e.win.mp3.streams.get(&a[0]) else {
        return Ok(u32::MAX);
    };
    let v = attribute(s, a[1]);
    if std::env::var_os("XP_MP3_LOG").is_some_and(|v| v == "2") {
        eprintln!(
            "[mp3] {:.3} {:#x} attribute {} -> {v:#x}",
            e.win.elapsed_ms() as f64 / 1000.0,
            a[0],
            a[1]
        );
    }
    Ok(v)
}

fn attribute(s: &Mp3Stream, token: u32) -> u32 {
    match token {
        INPUT_BIT_RATE => s.bitrate,
        INPUT_SAMPLE_RATE | OUTPUT_SAMPLE_RATE => s.rate,
        // As the plugin computes it: compressed bits per sample, at least 1.
        INPUT_SAMPLE_WIDTH => {
            let ratio = (s.rate * s.channels * 16)
                .checked_div(s.bitrate)
                .unwrap_or(0)
                .max(1);
            16 / ratio
        }
        INPUT_CHANNELS | OUTPUT_CHANNELS => s.channels,
        OUTPUT_BIT_RATE => s.rate * s.channels * 16,
        OUTPUT_SAMPLE_WIDTH => 16,
        POSITION => s.decoded_position(),
        PERCENT_DONE => {
            if s.total == 0 {
                0
            } else {
                (s.decoded_position() as f32 * 100.0 / s.total as f32).to_bits()
            }
        }
        MIN_INPUT_BLOCK => FETCH_BYTES,
        MPEG_VERSION => 1,
        MPEG_LAYER => s.layer,
        t @ FIRST_PREFERENCE..=19 => s.prefs[(t - FIRST_PREFERENCE) as usize],
        _ => u32::MAX,
    }
}

pub fn stream_set_preference(e: &mut Emu, a: &Args) -> Res<u32> {
    let (h, token, value) = (a[0], a[1], a[2]);
    if !(FIRST_PREFERENCE..=19).contains(&token) {
        return Ok(u32::MAX);
    }
    let v = e.mem.read(value, 4)?;
    let Some(s) = e.win.mp3.streams.get_mut(&h) else {
        return Ok(u32::MAX);
    };
    Ok(std::mem::replace(
        &mut s.prefs[(token - FIRST_PREFERENCE) as usize],
        v,
    ))
}

#[cfg(test)]
mod tests;
