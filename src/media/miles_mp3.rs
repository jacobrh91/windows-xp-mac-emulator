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

use std::collections::HashMap;

use nanomp3_core::{DecodeError, Decoder, MAX_SAMPLES_PER_FRAME};

use crate::cpu::{Emu, Res};
use crate::win32::Args;

/// How much compressed data the plugin asks for at a time (its "Minimum input block size").
const FETCH_BYTES: u32 = 4096;
/// Keep this much compressed input buffered: frames are only accepted once the next one is seen.
const INPUT_AHEAD: usize = 16 * 1024;

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
    /// Compressed bytes received so far (the "Position" attribute).
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
}

impl Mp3Stream {
    /// Ask the guest for more compressed data. Returns false at the end of the stream.
    fn fetch_more(&mut self, e: &mut Emu) -> Res<bool> {
        let got = e.call_function(
            self.fetch,
            &[
                self.user,
                self.scratch,
                FETCH_BYTES,
                self.next_offset as u32,
            ],
        )? as i32;
        self.next_offset = -1;
        if got <= 0 {
            self.eof = true;
            return Ok(false);
        }
        let got = (got as u32).min(FETCH_BYTES);
        let bytes = e.mem.read_bytes(self.scratch, got)?;
        // Drop what has been decoded before appending, so the buffer stays small.
        self.input.drain(..self.used);
        self.used = 0;
        self.input.extend_from_slice(&bytes);
        self.position = self.position.wrapping_add(got);
        Ok(true)
    }

    /// Decode one more frame into `pcm`. Returns false when nothing more will come.
    fn decode_frame(&mut self, e: &mut Emu) -> Res<bool> {
        loop {
            while !self.eof && self.input.len() - self.used < INPUT_AHEAD {
                self.fetch_more(e)?;
            }
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
            "[mp3] stream {handle:#x} opened: {} Hz, {} channels, {} kbit/s",
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
    if let Some(s) = e.win.mp3.streams.remove(&a[0]) {
        e.heap_free(s.scratch);
        e.heap_free(a[0]);
    }
    Ok(0)
}

pub fn stream_attribute(e: &mut Emu, a: &Args) -> Res<u32> {
    let Some(s) = e.win.mp3.streams.get(&a[0]) else {
        return Ok(u32::MAX);
    };
    Ok(attribute(s, a[1]))
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
        POSITION => s.position,
        PERCENT_DONE => {
            if s.total == 0 {
                0
            } else {
                (s.position as f32 * 100.0 / s.total as f32).to_bits()
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
mod tests {
    use super::*;
    use crate::win32::MAX_ARGS;

    fn call(e: &mut Emu, h: crate::win32::Handler, args: &[u32]) -> u32 {
        let mut full = [0u32; MAX_ARGS];
        full[..args.len()].copy_from_slice(args);
        h(e, &full).unwrap()
    }

    /// A tiny MP3: 1 s of a 440 Hz tone, made by ffmpeg if it is installed.
    fn sample_mp3() -> Option<Vec<u8>> {
        let ffmpeg = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())?;
        let out = std::process::Command::new(ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1:sample_rate=22050",
                "-ac",
                "2",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "64k",
                "-f",
                "mp3",
                "-",
            ])
            .output()
            .ok()?;
        (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
    }

    /// Guest code for the fetch callback, stdcall (user, dest, bytes, offset): copies from the
    /// MP3 held at 0x40000 (length at 0x3FFF8, read position at 0x3FFFC) and returns the count.
    fn install_fetch(e: &mut Emu, data: &[u8]) -> u32 {
        e.mem.map(0x30000, 0x80000);
        e.mem.write(0x3FFF8, 4, data.len() as u32).unwrap();
        e.mem.write(0x3FFFC, 4, 0).unwrap();
        e.mem.write_bytes(0x40000, data).unwrap();
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x56, 0x57,                         // push esi; push edi
            0x8B, 0x44, 0x24, 0x18,             // mov eax, [esp+24]   (offset)
            0x83, 0xF8, 0xFF,                   // cmp eax, -1
            0x74, 0x05,                         // je +5
            0xA3, 0xFC, 0xFF, 0x03, 0x00,       // mov [0x3FFFC], eax
            0x8B, 0x0D, 0xF8, 0xFF, 0x03, 0x00, // mov ecx, [0x3FFF8]   (length)
            0x2B, 0x0D, 0xFC, 0xFF, 0x03, 0x00, // sub ecx, [0x3FFFC]   (remaining)
            0x3B, 0x4C, 0x24, 0x14,             // cmp ecx, [esp+20]    (bytes wanted)
            0x7E, 0x04,                         // jle +4
            0x8B, 0x4C, 0x24, 0x14,             // mov ecx, [esp+20]
            0x8B, 0x7C, 0x24, 0x10,             // mov edi, [esp+16]    (dest)
            0x8B, 0x35, 0xFC, 0xFF, 0x03, 0x00, // mov esi, [0x3FFFC]
            0x81, 0xC6, 0x00, 0x00, 0x04, 0x00, // add esi, 0x40000
            0x8B, 0xC1,                         // mov eax, ecx
            0x01, 0x05, 0xFC, 0xFF, 0x03, 0x00, // add [0x3FFFC], eax
            0xF3, 0xA4,                         // rep movsb
            0x5F, 0x5E,                         // pop edi; pop esi
            0xC2, 0x10, 0x00,                   // ret 16
        ];
        e.mem.write_bytes(0x30000, &code).unwrap();
        crate::loader::init_process(e).unwrap();
        0x30000
    }

    #[test]
    fn decodes_an_mp3_through_the_guest_fetch_callback() {
        let Some(mp3) = sample_mp3() else { return };
        let mut e = Emu::new();
        let fetch = install_fetch(&mut e, &mp3);
        let h = call(&mut e, stream_open, &[7, fetch, mp3.len() as u32]);
        assert_ne!(h, 0, "recognised as MP3");
        let s = &e.win.mp3.streams[&h];
        assert_eq!(
            (
                attribute(s, OUTPUT_SAMPLE_RATE),
                attribute(s, OUTPUT_CHANNELS),
                attribute(s, OUTPUT_SAMPLE_WIDTH)
            ),
            (22050, 2, 16)
        );
        // Pull it all out in Miles-sized pieces.
        let buf = 0x60000;
        let mut pcm = Vec::new();
        loop {
            let n = call(&mut e, stream_process, &[h, buf, 4096]);
            pcm.extend(e.mem.read_bytes(buf, n).unwrap());
            if n < 4096 {
                break;
            }
        }
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let frames = samples.len() / 2;
        assert!(
            (21000..=24500).contains(&frames),
            "about one second at 22.05 kHz, got {frames} frames"
        );
        assert!(
            samples.iter().any(|&v| v.abs() > 2000),
            "the tone (ffmpeg makes it at 1/8 of full scale), not silence"
        );
        let percent = f32::from_bits(attribute(&e.win.mp3.streams[&h], PERCENT_DONE));
        assert!(
            (99.0..=100.0).contains(&percent),
            "all input consumed: {percent}%"
        );
        // Seek back to the start and decode again.
        call(&mut e, stream_seek, &[h, 0]);
        assert_eq!(call(&mut e, stream_process, &[h, buf, 4096]), 4096);
        assert_eq!(call(&mut e, stream_close, &[h]), 0);
        assert!(e.win.mp3.streams.is_empty());
    }

    #[test]
    fn data_that_is_not_mp3_is_refused() {
        let mut e = Emu::new();
        let fetch = install_fetch(&mut e, &[0x55; 3000]);
        assert_eq!(call(&mut e, stream_open, &[7, fetch, 3000]), 0);
    }
}
