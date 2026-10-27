//! Tests of `miles_mp3`.

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
    call(&mut e, stream_process, &[h, buf, 4096]);
    let s = &e.win.mp3.streams[&h];
    assert!(
        attribute(s, POSITION) < mp3.len() as u32 / 4,
        "a little decoded: the position counts what was decoded, not what was read ahead ({} of {})",
        attribute(s, POSITION),
        mp3.len()
    );
    call(&mut e, stream_seek, &[h, 0]);
    assert_eq!(call(&mut e, stream_process, &[h, buf, 4096]), 4096);
    assert_eq!(call(&mut e, stream_close, &[h]), 0);
    assert!(e.win.mp3.streams.is_empty());
}

#[test]
fn frame_lengths_follow_the_header() {
    assert_eq!(
        frame_length(&[0xFF, 0xFB, 0x90, 0]),
        Some(417),
        "MPEG-1 III 128 kbit/s 44.1 kHz"
    );
    assert_eq!(frame_length(&[0xFF, 0xFB, 0x92, 0]), Some(418), "padded");
    assert_eq!(
        frame_length(&[0xFF, 0xF3, 0x80, 0]),
        Some(208),
        "MPEG-2 III 64 kbit/s 22.05 kHz"
    );
    assert_eq!(frame_length(&[0xFF, 0xFB, 0x00, 0]), Some(0), "free format");
    assert_eq!(frame_length(&[0x49, 0x44, 0x33, 3]), None);
}

/// The decoder reads only as far as the frame it is decoding: Miles ends a stream as soon
/// as its file has been read, so read-ahead cut the end off every sound.
#[test]
fn input_is_read_a_frame_at_a_time() {
    let Some(mp3) = sample_mp3() else { return };
    let mut e = Emu::new();
    let fetch = install_fetch(&mut e, &mp3);
    let h = call(&mut e, stream_open, &[7, fetch, mp3.len() as u32]);
    let buf = 0x60000;
    let mut total = 0;
    loop {
        let n = call(&mut e, stream_process, &[h, buf, 1024]);
        total += n;
        let s = &e.win.mp3.streams[&h];
        assert!(
            s.buffered() <= 1441 + 4,
            "{} bytes read ahead",
            s.buffered()
        );
        let read = e.mem.read(0x3FFFC, 4).unwrap() as usize;
        if read == mp3.len() {
            // The file is read to its end only for its last frame or two.
            let left = (s.pcm.len() - s.pcm_used) as u32 + 2 * 1152 * 2 * 2;
            let whole = 22050 * 2 * 2;
            assert!(
                total + left >= whole * 95 / 100,
                "end of file reached early: {total} bytes out"
            );
            break;
        }
        assert_eq!(n, 1024);
    }
}

#[test]
fn data_that_is_not_mp3_is_refused() {
    let mut e = Emu::new();
    let fetch = install_fetch(&mut e, &[0x55; 3000]);
    assert_eq!(call(&mut e, stream_open, &[7, fetch, 3000]), 0);
}
