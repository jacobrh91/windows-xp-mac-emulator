//! Benchmarks (run with `cargo test --release bench_loop -- --ignored --nocapture`).

use super::*;

/// `cargo test --release bench_loop -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_loop() {
    // loop: add eax,[esp+4]; mov [esp+8],eax; lea edx,[eax+ecx*2]; cmp edx,ebx; dec ecx; jnz loop
    let code: Vec<u8> = vec![
        0x03, 0x44, 0x24, 0x04, // add eax,[esp+4]
        0x89, 0x44, 0x24, 0x08, // mov [esp+8],eax
        0x8D, 0x14, 0x48, // lea edx,[eax+ecx*2]
        0x39, 0xDA, // cmp edx,ebx
        0x49, // dec ecx
        0x75, 0xF0, // jnz loop
    ];
    let mut e = Emu::new();
    e.mem.map(0x10000, 0x20000);
    e.mem.write_bytes(0x10000, &code).unwrap();
    e.eip = 0x10000;
    e.regs[ESP] = 0x20000;
    e.regs[ECX] = 10_000_000;
    let t = std::time::Instant::now();
    let stop = e.run(200_000_000);
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "{stop:?}: {} steps in {secs:.2}s = {:.1}M steps/s",
        e.steps,
        e.steps as f64 / secs / 1e6
    );
}
