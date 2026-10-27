//! Tests of the instruction cache: decoding, and that executing decoded instructions (cached,
//! in blocks, or compiled) gives the same results as the interpreter.

use super::decode::*;
use super::*;
use crate::cpu::{EAX, EBX, ESI, ESP, Stop};

/// Every instruction the block decoder accepts runs the same through `exec_dop` as through
/// the full interpreter: random instruction bytes, registers, flags and memory, compared
/// afterwards (registers, flags, eip, the memory window, and whether it faulted).
#[test]
fn decoded_instructions_match_the_full_interpreter() {
    const CODE: u32 = 0x1000;
    const DATA: u32 = 0x10_0000;
    let mut x = 0x9E37_79B9u32;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    let mut tried = 0;
    let mut mismatches = Vec::new();
    let (mut a, mut b) = (Emu::new(), Emu::new());
    for e in [&mut a, &mut b] {
        e.mem.map(CODE, 0x1000);
        e.mem.map(DATA, 0x1_0000);
    }
    // (Far fewer unoptimized: the full run takes minutes in a debug build.)
    let tries = if cfg!(debug_assertions) {
        30_000
    } else {
        300_000
    };
    for _ in 0..tries {
        let mut bytes = [0u8; 16];
        for b in bytes.iter_mut() {
            *b = rnd() as u8;
        }
        // Mostly small displacements so memory operands land in the mapped window.
        if rnd() % 4 != 0 {
            for b in bytes[2..8].iter_mut() {
                if rnd() % 2 == 0 {
                    *b = 0;
                }
            }
        }
        // Two-byte and operand-size forms come up rarely at random: make some.
        match rnd() % 8 {
            0 => {
                const OPS: [u8; 7] = [0xAF, 0xB6, 0xB7, 0xBE, 0xBF, 0x80, 0x90];
                let o = OPS[(rnd() % 7) as usize];
                bytes[0] = 0x0F;
                bytes[1] = if o >= 0x80 { o + (rnd() % 16) as u8 } else { o };
            }
            1 => bytes[0] = 0x66,
            _ => {}
        }
        let Some((len, op)) = decode(&bytes, CODE) else {
            continue;
        };
        if op == Dop::Unsupported {
            continue;
        }
        tried += 1;
        let mut regs = [0u32; 8];
        for r in regs.iter_mut() {
            *r = DATA + 0x8000 + (rnd() % 0x1000);
            if rnd() % 3 == 0 {
                *r = rnd(); // arbitrary values too (faults compared as well)
            }
        }
        regs[ESP as usize] = DATA + 0xC000;
        let flags = (rnd() & 0x8D5) | 2 | (rnd() & 0x400);
        let mem_seed = rnd();
        let mut fpu_st = [0f64; 8];
        for v in fpu_st.iter_mut() {
            *v = match rnd() % 4 {
                0 => (rnd() as i32) as f64,
                1 => (rnd() as i32) as f64 / 7.0,
                2 => (rnd() % 100) as f64 - 50.5,
                _ => f32::from_bits(rnd()) as f64,
            };
        }
        let fpu_top = (rnd() % 8) as usize;
        let setup = |e: &mut Emu| {
            e.mem.write_bytes(CODE, &bytes).unwrap();
            let mut y = mem_seed;
            for i in 0..0x4000 {
                y = y.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                e.mem.write(DATA + 4 * i, 4, y).unwrap();
            }
            e.regs = regs;
            e.eflags = flags;
            e.eip = CODE;
            e.fpu.st = fpu_st;
            e.fpu.top = fpu_top;
            e.fpu.cc = 0;
        };
        setup(&mut a);
        let ra = a.exec_dop(op, CODE, CODE + len as u32);
        setup(&mut b);
        let rb = b.step_decoded(CODE);
        let state = |e: &Emu, r: &Res<()>| {
            (
                r.is_ok(),
                if r.is_ok() { e.regs } else { [0; 8] },
                if r.is_ok() { e.eflags & 0x8D5 } else { 0 },
                if r.is_ok() { e.eip } else { 0 },
                e.mem.read_bytes(DATA, 0x1_0000).unwrap(),
                if r.is_ok() {
                    e.fpu.st.map(f64::to_bits)
                } else {
                    [0; 8]
                },
                if r.is_ok() {
                    (e.fpu.top, e.fpu.cc)
                } else {
                    (0, 0)
                },
            )
        };
        if state(&a, &ra) != state(&b, &rb) && mismatches.len() < 20 {
            let (sa, sb) = (state(&a, &ra), state(&b, &rb));
            mismatches.push(format!(
                "{:02x?} {op:?}: ok {}/{} regs {:x?}/{:x?} flags {:#x}/{:#x} eip {:#x}/{:#x} mem same {} fpu same {} top/cc {:?}/{:?}",
                &bytes[..len as usize], sa.0, sb.0, sa.1, sb.1, sa.2, sb.2, sa.3, sb.3, sa.4 == sb.4, sa.5 == sb.5, sa.6, sb.6
            ));
        }
    }
    assert!(
        tried > tries / 4,
        "only {tried} decodable instructions tried"
    );
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The instructions added for blocks (8-bit immediates, setcc, cdq, the moffs forms, 16-bit
/// mov and ALU, not/neg, test with an immediate, movsb/movsd both ways) give the same
/// registers, flags and memory compiled as interpreted: a loop runs them until it is hot.
#[test]
fn added_instructions_compile_like_the_interpreter() {
    const CODE: u32 = 0x1000;
    const M: u32 = 0x3000;
    const SRC: u32 = 0x4000;
    const DST: u32 = 0x5000;
    let le = |v: u32| v.to_le_bytes().to_vec();
    let code: Vec<u8> = [
        vec![0xBE],
        le(SRC),
        vec![0xBF],
        le(DST),                      // mov esi, SRC; mov edi, DST
        vec![0xB0, 0x7F, 0xB4, 0x80], // mov al, 0x7F; mov ah, 0x80
        vec![0xC6, 0x05],
        le(M),
        vec![0x5A],             // mov byte [M], 0x5A
        vec![0x0F, 0x9C, 0xC1], // setl cl
        vec![0x0F, 0x94, 0x05],
        le(M + 1),  // sete byte [M+1]
        vec![0x99], // cdq
        vec![0x66, 0x8B, 0x1D],
        le(M + 4), // mov bx, [M+4]
        vec![0x66, 0x89, 0x1D],
        le(M + 8),              // mov [M+8], bx
        vec![0x66, 0x89, 0xD9], // mov cx, bx
        vec![0x66, 0x03, 0x05],
        le(M + 4),                    // add ax, [M+4]
        vec![0x66, 0x83, 0xC1, 0xF0], // add cx, -16
        vec![0xA1],
        le(M + 12),       // mov eax, [M+12]
        vec![0xF7, 0xD8], // neg eax
        vec![0xA3],
        le(M + 16),             // mov [M+16], eax
        vec![0xF7, 0xD3],       // not ebx
        vec![0xF6, 0xC3, 0x81], // test bl, 0x81
        vec![0x0F, 0x9F, 0x05],
        le(M + 2), // setg byte [M+2]
        vec![0xF7, 0x05],
        le(M + 4),
        le(0xF0F), // test dword [M+4], 0xF0F
        vec![0x0F, 0x92, 0x05],
        le(M + 3), // setb byte [M+3]
        vec![0xF7, 0x1D],
        le(M + 20),                   // neg dword [M+20]
        vec![0xA5, 0xA4],             // movsd; movsb
        vec![0xFD, 0xA5, 0xA4, 0xFC], // std; movsd; movsb; cld
        vec![0xA5],                   // movsd
        vec![0x4D, 0x0F, 0x85],       // dec ebp; jnz to the top
    ]
    .concat();
    let mut code = code;
    let back = -(code.len() as i32 + 4);
    code.extend(le(back as u32));
    code.push(0xF4);
    let run = |jit: bool, seed: u32| {
        let mut e = Emu::new();
        e.jit.enabled &= jit;
        for (a, n) in [(CODE, 0x1000), (M, 0x1000), (SRC, 0x1000), (DST, 0x1000)] {
            e.mem.map(a, n);
        }
        e.mem.write_bytes(CODE, &code).unwrap();
        let mut x = seed;
        for i in 0..0x400 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            for base in [M, SRC] {
                e.mem.write(base + 4 * i, 4, x ^ base).unwrap();
            }
        }
        e.regs = [seed, 3, 0x8000_0000, seed >> 3, 0x2000, 40, 0, 0];
        e.eip = CODE;
        assert_eq!(e.run(100_000), Stop::Halt);
        if jit {
            assert!(e.jit.compiled > 0, "the loop was compiled");
        }
        let mem: Vec<u8> = [M, SRC, DST]
            .iter()
            .flat_map(|&a| e.mem.read_bytes(a, 0x200).unwrap())
            .collect();
        (e.regs, e.eflags & 0x8D5, mem)
    };
    for seed in [0, 1, 0x8000_0000, 0x1234_5678, 0xFFFF_FFFF] {
        assert_eq!(run(true, seed), run(false, seed), "seed {seed:#x}");
    }
}

#[test]
fn decodes_operands_and_absolute_targets() {
    // mov eax, [ebx+esi*4+0x10]
    assert_eq!(
        decode(&[0x8B, 0x44, 0xB3, 0x10], 0),
        Some((
            4,
            Dop::MovRRm {
                reg: 0,
                rm: Ea::Mem {
                    base: 3,
                    index: 6,
                    scale: 2,
                    disp: 0x10
                },
                bits: 32
            }
        ))
    );
    // jne -2 at 0x1000: back to itself
    assert_eq!(
        decode(&[0x75, 0xFE], 0x1000),
        Some((
            2,
            Dop::Jcc {
                cc: 5,
                target: 0x1000
            }
        ))
    );
    // call +0x100 at 0x2000
    assert_eq!(
        decode(&[0xE8, 0x00, 0x01, 0, 0], 0x2000),
        Some((5, Dop::Call(0x2105)))
    );
    // [disp32] with no base
    assert_eq!(
        decode(&[0x8B, 0x05, 0x78, 0x56, 0x34, 0x12], 0).map(|d| d.1),
        Some(Dop::MovRRm {
            reg: 0,
            rm: Ea::Mem {
                base: NONE,
                index: NONE,
                scale: 0,
                disp: 0x1234_5678
            },
            bits: 32
        })
    );
    assert_eq!(
        decode(&[0x66, 0x89, 0xC8], 0),
        Some((
            3,
            Dop::MovRmR {
                rm: Ea::Reg(0),
                reg: 1,
                bits: 16
            }
        )),
        "operand-size prefix: the 16-bit form"
    );
    assert_eq!(
        decode(&[0xF3, 0xA5], 0),
        None,
        "rep: left to the interpreter"
    );
    assert_eq!(
        decode(&[0xD9, 0xE8], 0),
        Some((
            2,
            Dop::Fpu {
                opc: 0xD9,
                reg: 5,
                rm: Ea::Reg(0)
            }
        )),
        "fld1, ModRM decoded"
    );
    assert_eq!(decode(&[0x8B], 0), None, "truncated");
}

/// Tiny random generator (no external crates in tests).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.next() as usize % v.len()]
    }
}

/// A random instruction the cache handles, touching memory only through [ebx+disp8] /
/// [esi+disp8] (both kept pointing into the data area) and never writing ebx, esi or esp.
fn random_insn(r: &mut Rng, out: &mut Vec<u8>) {
    let dst_reg = |r: &mut Rng| r.pick(&[0u8, 1, 2, 5, 7]); // eax ecx edx ebp edi
    let modrm_reg_mem = |r: &mut Rng, reg: u8| -> Vec<u8> {
        if r.next() % 2 == 0 {
            vec![0xC0 | (reg << 3) | r.pick(&[0u8, 1, 2, 3, 5, 6, 7])]
        } else {
            let base = r.pick(&[3u8, 6]);
            vec![0x40 | (reg << 3) | base, (r.next() % 0x7C) as u8 & !3]
        }
    };
    // destination is a written operand: register form only for our "written" regs, or memory
    let modrm_dst = |r: &mut Rng, reg: u8| -> Vec<u8> {
        if r.next().is_multiple_of(2) {
            vec![0xC0 | (reg << 3) | dst_reg(r)]
        } else {
            let base = r.pick(&[3u8, 6]);
            vec![0x40 | (reg << 3) | base, (r.next() % 0x7C) as u8 & !3]
        }
    };
    match r.next() % 18 {
        15..=17 => {
            // x87: memory forms at [ebx/esi + d8] and register forms
            let mem = |r: &mut Rng, op: u8| {
                vec![
                    0x40 | (op << 3) | r.pick(&[3u8, 6]),
                    (r.next() % 0x78) as u8 & !7,
                ]
            };
            match r.next() % 9 {
                0 => {
                    out.push(r.pick(&[0xD9u8, 0xDD]));
                    let op = r.pick(&[0u8, 2, 3]);
                    out.extend(mem(r, op));
                }
                1 => {
                    out.push(r.pick(&[0xD8u8, 0xDC]));
                    let op = r.pick(&[0u8, 1, 4, 5, 6, 7]);
                    out.extend(mem(r, op));
                }
                2 | 3 => {
                    let opc = r.pick(&[0xD8u8, 0xDC, 0xDE]);
                    let op = r.pick(&[0u8, 1, 4, 5, 6, 7]);
                    out.extend([opc, 0xC0 | (op << 3) | (r.next() % 8) as u8]);
                }
                4 => out.extend([0xD9, 0xC0 | (r.next() % 8) as u8]), // fld st(i)
                5 => out.extend([0xD9, 0xC8 | (r.next() % 8) as u8]), // fxch
                6 => out.extend([0xDD, r.pick(&[0xD0u8, 0xD8]) | (r.next() % 8) as u8]), // fst/fstp st(i)
                7 => match r.next() % 4 {
                    0 => out.extend([0xD9, r.pick(&[0xE0u8, 0xE1, 0xE8, 0xEE])]),
                    1 => {
                        // fcom / fcomp m32 / m64, then fnstsw ax
                        out.push(r.pick(&[0xD8u8, 0xDC]));
                        let op = r.pick(&[2u8, 3]);
                        out.extend(mem(r, op));
                        out.extend([0xDF, 0xE0]);
                    }
                    2 => {
                        out.extend([0xD8, r.pick(&[0xD0u8, 0xD8]) | (r.next() % 8) as u8]);
                        out.extend([0xDF, 0xE0]);
                    }
                    _ => {
                        out.push(r.pick(&[0xDBu8, 0xDF])); // fild m32 / m16
                        out.extend(mem(r, 0));
                    }
                },
                _ => {
                    // a value to work with: fild-free, so load a float from memory
                    out.push(0xDD);
                    out.extend(mem(r, 0));
                }
            }
        }
        12 => {
            // 8-bit mov in either direction
            out.push(r.pick(&[0x88u8, 0x8A]));
            let reg = r.pick(&[0u8, 1, 2, 4, 5, 6]); // al cl dl ah ch dh
            out.extend(modrm_dst(r, reg));
        }
        13 => {
            // 8-bit alu with an immediate, or test r/m8, r8
            if r.next().is_multiple_of(2) {
                out.push(0x80);
                let op = (r.next() % 8) as u8;
                out.extend(modrm_dst(r, op));
                out.push(r.next() as u8);
            } else {
                out.push(0x84);
                let reg = r.pick(&[0u8, 1, 2, 3, 4, 5, 6, 7]);
                out.extend(modrm_dst(r, reg));
            }
        }
        14 => {
            // al, imm8 forms and imul with an immediate
            if r.next().is_multiple_of(2) {
                out.extend([((r.next() % 8) as u8) << 3 | 4, r.next() as u8]);
            } else {
                out.push(0x6B);
                let reg = dst_reg(r);
                out.extend(modrm_reg_mem(r, reg));
                out.push(r.next() as u8);
            }
        }
        0 => {
            // alu r/m, r (dst may be memory)
            let op = (r.next() % 8) as u8;
            out.push(op << 3 | r.pick(&[0u8, 1]));
            let reg = r.pick(&[0u8, 1, 2, 3, 5, 6, 7]);
            out.extend(modrm_dst(r, reg));
        }
        1 => {
            let op = (r.next() % 8) as u8;
            out.push(op << 3 | r.pick(&[2u8, 3]));
            let reg = dst_reg(r);
            out.extend(modrm_reg_mem(r, reg));
        }
        2 => {
            out.push(r.pick(&[0x81u8, 0x83]));
            let op = (r.next() % 8) as u8;
            let rest = modrm_dst(r, op);
            let imm8 = out[out.len() - 1] == 0x83;
            out.extend(rest);
            if imm8 {
                out.push(r.next() as u8);
            } else {
                out.extend(r.next().to_le_bytes());
            }
        }
        3 => {
            out.push(0x8B);
            let reg = dst_reg(r);
            out.extend(modrm_reg_mem(r, reg));
        }
        4 => {
            out.push(0x89);
            let reg = r.pick(&[0u8, 1, 2, 3, 5, 6, 7]);
            out.extend(modrm_dst(r, reg));
        }
        5 => {
            out.push(0x8D);
            let reg = dst_reg(r);
            out.extend([0x44 | (reg << 3), 0x73, r.next() as u8]); // lea reg, [ebx+esi*2+d8]
        }
        6 => out.push(r.pick(&[0x40u8, 0x41, 0x42, 0x45, 0x47, 0x48, 0x49, 0x4A, 0x4D, 0x4F])),
        7 => {
            let opc = r.pick(&[0xC1u8, 0xD1, 0xD3]);
            out.push(opc);
            let op = r.pick(&[0u8, 1, 4, 5, 7]);
            out.extend(modrm_dst(r, op));
            if opc == 0xC1 {
                out.push((r.next() % 40) as u8);
            }
        }
        8 => {
            out.extend([0x0F, r.pick(&[0xB6u8, 0xB7, 0xBE, 0xBF])]);
            let reg = dst_reg(r);
            out.extend(modrm_reg_mem(r, reg));
        }
        9 => {
            out.extend([0x0F, 0xAF]);
            let reg = dst_reg(r);
            out.extend(modrm_reg_mem(r, reg));
        }
        10 => {
            // push then pop into a written register
            out.push(0x50 | r.pick(&[0u8, 1, 2, 3, 5, 6, 7]));
            out.push(0x58 | dst_reg(r));
        }
        _ => {
            // a conditional skip over a 1-byte inc, so both directions run
            out.extend([0x70 | (r.next() % 16) as u8, 0x01, 0x40 | dst_reg(r)]);
        }
    }
}

fn machine(code: &[u8], seed: u64, cached: bool) -> Emu {
    let mut e = Emu::new();
    e.icache_on = cached;
    e.mem.map(0x1000, 0x20000);
    e.mem.write_bytes(0x1000, code).unwrap();
    e.mem
        .write_bytes(0x1000 + code.len() as u32, &[0xF4])
        .unwrap(); // hlt
    let mut r = Rng(seed);
    for a in (0x10000..0x10400).step_by(4) {
        e.mem.write(a, 4, r.next()).unwrap();
    }
    for (i, v) in e.regs.iter_mut().enumerate() {
        *v = r.next();
        if i == EBX {
            *v = 0x10000;
        }
        if i == ESI {
            *v = 0x10100;
        }
    }
    e.regs[ESP] = 0x18000;
    e.eflags = 2 | (r.next() & 0x8D5);
    e.eip = 0x1000;
    e
}

#[test]
fn cached_execution_matches_the_interpreter_on_random_code() {
    for seed in 1..=60u64 {
        let mut r = Rng(seed * 7919);
        let mut code = Vec::new();
        for _ in 0..200 {
            random_insn(&mut r, &mut code);
        }
        // Run it twice over (a loop back to the start) so cached entries are reused.
        let mut a = machine(&code, seed, false);
        let mut b = machine(&code, seed, true);
        for _ in 0..2 {
            a.eip = 0x1000;
            b.eip = 0x1000;
            assert_eq!(a.run(10_000), Stop::Halt);
            assert_eq!(b.run(10_000), Stop::Halt);
        }
        assert_eq!(a.regs, b.regs, "registers (seed {seed})");
        assert_eq!(a.eflags, b.eflags, "flags (seed {seed})");
        assert_eq!(a.steps, b.steps, "instruction count (seed {seed})");
        assert_eq!(
            a.mem.read_bytes(0x10000, 0x400).unwrap(),
            b.mem.read_bytes(0x10000, 0x400).unwrap(),
            "memory (seed {seed})"
        );
        assert_eq!(
            a.mem.read_bytes(0x17F00, 0x100).unwrap(),
            b.mem.read_bytes(0x17F00, 0x100).unwrap(),
            "stack (seed {seed})"
        );
    }
}

/// The same random code, run often enough for its blocks to be compiled, with the JIT
/// against the plain interpreter.
#[test]
fn compiled_blocks_match_the_interpreter_on_random_code() {
    let mut compiled = 0;
    for seed in 1..=40u64 {
        let mut r = Rng(seed * 104_729);
        let mut code = Vec::new();
        for _ in 0..120 {
            random_insn(&mut r, &mut code);
        }
        let mut a = machine(&code, seed, false);
        a.jit.enabled = false;
        let mut b = machine(&code, seed, true);
        if !b.jit.enabled {
            return; // not an Apple Silicon Mac
        }
        for pass in 0..40 {
            a.eip = 0x1000;
            b.eip = 0x1000;
            assert_eq!(a.run(100_000), Stop::Halt);
            assert_eq!(b.run(100_000), Stop::Halt);
            assert_eq!(a.regs, b.regs, "registers (seed {seed}, pass {pass})");
            assert_eq!(a.eflags, b.eflags, "flags (seed {seed}, pass {pass})");
            assert_eq!(
                a.fpu.top, b.fpu.top,
                "x87 stack top (seed {seed}, pass {pass})"
            );
            assert_eq!(
                a.fpu.st.map(f64::to_bits),
                b.fpu.st.map(f64::to_bits),
                "x87 registers (seed {seed}, pass {pass})"
            );
        }
        assert_eq!(a.steps, b.steps, "instruction count (seed {seed})");
        assert_eq!(
            a.mem.read_bytes(0x10000, 0x400).unwrap(),
            b.mem.read_bytes(0x10000, 0x400).unwrap(),
            "memory (seed {seed})"
        );
        assert_eq!(
            a.mem.read_bytes(0x17F00, 0x100).unwrap(),
            b.mem.read_bytes(0x17F00, 0x100).unwrap(),
            "stack (seed {seed})"
        );
        compiled += b.jit.compiled;
    }
    assert!(compiled > 0, "blocks were actually compiled");
}

#[test]
fn rewriting_code_drops_the_stale_decoding() {
    // mov eax, 1 ; hlt -- run, patch to mov eax, 2, run again
    let mut e = machine(&[0xB8, 1, 0, 0, 0], 1, true);
    assert_eq!(e.run(100), Stop::Halt);
    assert_eq!(e.regs[EAX], 1);
    e.mem.write(0x1001, 1, 2).unwrap();
    e.eip = 0x1000;
    assert_eq!(e.run(100), Stop::Halt);
    assert_eq!(
        e.regs[EAX], 2,
        "the new instruction ran, not the cached one"
    );
}

#[test]
fn faults_inside_cached_instructions_are_precise() {
    // mov [0x00000010], eax -- unmapped: the fault leaves eip on the instruction
    let mut e = machine(&[0x89, 0x05, 0x10, 0, 0, 0], 1, true);
    let stop = e.run(100);
    assert!(matches!(stop, Stop::Fault(_)), "{stop:?}");
    assert_eq!(e.eip, 0x1000);
}
