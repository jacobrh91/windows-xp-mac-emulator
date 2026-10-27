//! Tests of `the cpu module`.

use super::*;

const CODE: u32 = 0x1000;

/// Load `code` at 0x1000 (data scratch space at 0x2000..) and run until it stops.
fn run(code: &[u8]) -> (Emu, Stop) {
    let mut e = Emu::new();
    e.mem.map(CODE, 0x10000);
    e.mem.write_bytes(CODE, code).unwrap();
    e.eip = CODE;
    e.regs[ESP] = 0x10000;
    let stop = e.run(100_000);
    (e, stop)
}

/// rcl/rcr against a rotation one bit at a time, for every width and count.
#[test]
fn rotate_through_carry_matches_a_bitwise_reference() {
    let mut e = Emu::new();
    let mut seed = 0x9E37_79B9u32;
    for _ in 0..2000 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let bits = [8, 16, 32][(seed >> 3) as usize % 3];
        let a = seed.rotate_left(7) & mask(bits);
        let count = seed >> 27; // 0..=31
        let cf = seed & 1 != 0;
        for op in [2usize, 3] {
            let (mut v, mut c) = (a, cf);
            for _ in 0..count % (bits + 1) {
                let top = v >> (bits - 1) & 1 != 0;
                if op == 2 {
                    v = (v << 1 | c as u32) & mask(bits);
                    c = top;
                } else {
                    let low = v & 1 != 0;
                    v = v >> 1 | (c as u32) << (bits - 1);
                    c = low;
                }
            }
            e.set_flag(CF, cf);
            let r = e.shift(op, a, count, bits).unwrap();
            assert_eq!(
                (r, e.flag(CF)),
                (v, if count == 0 { cf } else { c }),
                "op {op} {bits}-bit {a:#x} by {count}, CF {cf}"
            );
        }
    }
    // OF for a single step
    e.set_flag(CF, false);
    assert_eq!(e.shift(2, 0x80, 1, 8).unwrap(), 0);
    assert!(
        e.flag(CF) && e.flag(OF),
        "rcl: top bit out to CF, which differs from the new top bit"
    );
    e.set_flag(CF, true);
    assert_eq!(e.shift(3, 1, 1, 32).unwrap(), 0x8000_0000);
    assert!(
        e.flag(CF) && e.flag(OF),
        "rcr: CF in at the top, next to a 0"
    );
}

fn run_ok(code: &[u8]) -> Emu {
    let (e, stop) = run(code);
    assert_eq!(stop, Stop::Halt, "program did not halt cleanly");
    e
}

#[test]
fn mov_add_sub() {
    let e = run_ok(&[0xB8, 5, 0, 0, 0, 0x83, 0xC0, 3, 0x83, 0xE8, 1, 0xF4]);
    assert_eq!(e.regs[EAX], 7);
}

#[test]
fn counted_loop_sums_1_to_10() {
    let e = run_ok(&[
        0xB9, 10, 0, 0, 0, // mov ecx, 10
        0x31, 0xC0, // xor eax, eax
        0x01, 0xC8, // L: add eax, ecx
        0x49, // dec ecx
        0x75, 0xFB, // jnz L
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 55);
    assert!(e.eflags & ZF != 0);
}

#[test]
fn call_and_ret() {
    let e = run_ok(&[
        0xE8, 1, 0, 0, 0,    // call f
        0xF4, // hlt
        0xB8, 0x34, 0x12, 0, 0, // f: mov eax, 0x1234
        0xC3,
    ]);
    assert_eq!(e.regs[EAX], 0x1234);
    assert_eq!(e.regs[ESP], 0x10000);
}

#[test]
fn push_pop() {
    let e = run_ok(&[0x68, 0x44, 0x33, 0x22, 0x11, 0x5B, 0xF4]);
    assert_eq!(e.regs[EBX], 0x11223344);
    assert_eq!(e.regs[ESP], 0x10000);
}

/// With an address-size prefix, loop counts in CX and leaves the top of ECX alone.
#[test]
fn loop_with_address_size_prefix_counts_cx() {
    let e = run_ok(&[
        0xB9, 0x03, 0x00, 0xAB, 0x12, // mov ecx, 0x12ab0003
        0x31, 0xC0, // xor eax, eax
        0x40, // inc eax          <- loop body
        0x67, 0xE2, 0xFC, // loop (cx) back to inc
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 3, "three passes, not 0x12ab0003");
    assert_eq!(e.regs[ECX], 0x12AB_0000);
    // loopnz stops early when ZF is set
    let e = run_ok(&[
        0xB9, 0x0A, 0x00, 0x00, 0x00, // mov ecx, 10
        0x31, 0xC0, // xor eax, eax
        0x40, // inc eax
        0x83, 0xF8, 0x04, // cmp eax, 4
        0xE0, 0xFA, // loopnz back to inc
        0xF4,
    ]);
    assert_eq!((e.regs[EAX], e.regs[ECX]), (4, 6));
}

/// With an operand-size prefix, pushes and pops move the stack by two bytes.
#[test]
fn sixteen_bit_push_and_pop() {
    let e = run_ok(&[
        0xB8, 0x44, 0x33, 0x22, 0x11, // mov eax, 0x11223344
        0xBB, 0x88, 0x77, 0x66, 0x55, // mov ebx, 0x55667788
        0x66, 0x50, // push ax
        0x66, 0x53, // push bx
        0x66, 0x68, 0xCD, 0xAB, // push word 0xabcd
        0x66, 0x59, // pop cx      (0xabcd)
        0x5A, // pop edx           (bx:ax = 0x77883344... as one dword)
        0xF4,
    ]);
    assert_eq!(
        e.regs[ECX] & 0xFFFF,
        0xABCD,
        "push imm16 reads two immediate bytes"
    );
    assert_eq!(e.regs[EDX], 0x3344_7788, "two word pushes make one dword");
    assert_eq!(e.regs[ESP], 0x10000, "balanced");
}

#[test]
fn unsigned_div_and_mul() {
    let e = run_ok(&[
        0xB8, 100, 0, 0, 0, 0xB9, 7, 0, 0, 0, 0x31, 0xD2, 0xF7, 0xF1, 0xF4,
    ]);
    assert_eq!((e.regs[EAX], e.regs[EDX]), (14, 2));

    let e = run_ok(&[0xB8, 0, 0, 1, 0, 0xB9, 0, 0, 1, 0, 0xF7, 0xE1, 0xF4]);
    assert_eq!((e.regs[EDX], e.regs[EAX]), (1, 0));
    assert!(e.eflags & CF != 0);
}

#[test]
fn signed_idiv() {
    // eax = -7, cdq, idiv 2 -> q = -3, r = -1
    let e = run_ok(&[
        0xB8, 0xF9, 0xFF, 0xFF, 0xFF, 0x99, 0xB9, 2, 0, 0, 0, 0xF7, 0xF9, 0xF4,
    ]);
    assert_eq!(e.regs[EAX] as i32, -3);
    assert_eq!(e.regs[EDX] as i32, -1);
}

#[test]
fn divide_by_zero_stops() {
    let (_, stop) = run(&[0x31, 0xD2, 0x31, 0xC9, 0xF7, 0xF1, 0xF4]);
    assert_eq!(stop, Stop::DivideError(CODE + 4));
}

#[test]
fn shifts() {
    let e = run_ok(&[
        0xB8, 0, 0, 0, 0x80, 0xC1, 0xF8, 4, // sar eax, 4
        0xBB, 1, 0, 0, 0, 0xC1, 0xE3, 31, // shl ebx, 31
        0xB9, 0, 0, 0, 0x80, 0xC1, 0xE9, 31, // shr ecx, 31
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 0xF800_0000);
    assert_eq!(e.regs[EBX], 0x8000_0000);
    assert_eq!(e.regs[ECX], 1);
}

#[test]
fn rep_stosd_fills_memory() {
    let e = run_ok(&[
        0xBF, 0x00, 0x20, 0, 0, // mov edi, 0x2000
        0xB9, 4, 0, 0, 0, // mov ecx, 4
        0xB8, 0xDD, 0xCC, 0xBB, 0xAA, // mov eax, 0xAABBCCDD
        0xFC, 0xF3, 0xAB, 0xF4,
    ]);
    for i in 0..4 {
        assert_eq!(e.mem.read(0x2000 + i * 4, 4).unwrap(), 0xAABB_CCDD);
    }
    assert_eq!(e.regs[EDI], 0x2010);
    assert_eq!(e.regs[ECX], 0);
}

#[test]
fn rep_movsb_copies() {
    let mut e = Emu::new();
    e.mem.map(CODE, 0x10000);
    e.mem.write_bytes(0x2000, b"abcdef").unwrap();
    e.mem
        .write_bytes(
            CODE,
            &[
                0xBE, 0, 0x20, 0, 0, 0xBF, 0, 0x30, 0, 0, 0xB9, 6, 0, 0, 0, 0xF3, 0xA4, 0xF4,
            ],
        )
        .unwrap();
    e.eip = CODE;
    assert_eq!(e.run(1000), Stop::Halt);
    assert_eq!(e.mem.read_bytes(0x3000, 6).unwrap(), b"abcdef");
}

#[test]
fn repne_scasb_finds_nul() {
    let mut e = Emu::new();
    e.mem.map(CODE, 0x10000);
    e.mem.write_bytes(0x2000, b"hello\0").unwrap();
    e.mem
        .write_bytes(
            CODE,
            &[
                0xBF, 0, 0x20, 0, 0, 0xB9, 0xFF, 0, 0, 0, 0x31, 0xC0, 0xF2, 0xAE, 0xF4,
            ],
        )
        .unwrap();
    e.eip = CODE;
    assert_eq!(e.run(1000), Stop::Halt);
    assert_eq!(e.regs[EDI], 0x2006);
}

#[test]
fn sib_addressing() {
    let e = run_ok(&[
        0xBB, 0, 0x20, 0, 0, // mov ebx, 0x2000
        0xBE, 2, 0, 0, 0, // mov esi, 2
        0xC7, 0x44, 0xB3, 0x08, 0xEF, 0xBE, 0xAD, 0xDE, // mov [ebx+esi*4+8], 0xdeadbeef
        0x8B, 0x44, 0xB3, 0x08, // mov eax, [ebx+esi*4+8]
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 0xDEAD_BEEF);
    assert_eq!(e.mem.read(0x2010, 4).unwrap(), 0xDEAD_BEEF);
}

#[test]
fn signed_vs_unsigned_conditions() {
    let e = run_ok(&[
        0xB8, 0xFF, 0xFF, 0xFF, 0xFF, // mov eax, -1
        0xBB, 1, 0, 0, 0, // mov ebx, 1
        0x39, 0xD8, // cmp eax, ebx
        0x0F, 0x9C, 0xC1, // setl cl
        0x0F, 0x92, 0xC2, // setb dl
        0xF4,
    ]);
    assert_eq!(e.regs[ECX] & 0xff, 1, "-1 < 1 signed");
    assert_eq!(e.regs[EDX] & 0xff, 0, "0xffffffff is not below 1 unsigned");
}

#[test]
fn cmov() {
    let e = run_ok(&[
        0xB8, 1, 0, 0, 0, 0xBB, 9, 0, 0, 0, 0x39, 0xC0, // cmp eax, eax (ZF=1)
        0x0F, 0x44, 0xC3, // cmove eax, ebx
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 9);
}

#[test]
fn movzx_movsx() {
    let e = run_ok(&[
        0xB8, 0x80, 0xFF, 0xFF, 0xFF, 0x0F, 0xBE, 0xD8, 0x0F, 0xB6, 0xC8, 0xF4,
    ]);
    assert_eq!(e.regs[EBX], 0xFFFF_FF80);
    assert_eq!(e.regs[ECX], 0x80);
}

#[test]
fn sixteen_bit_ops_preserve_upper_half() {
    let e = run_ok(&[
        0xB8, 0x78, 0x56, 0x34, 0x12, // mov eax, 0x12345678
        0x66, 0xB8, 0xFF, 0xFF, // mov ax, 0xffff
        0x66, 0x83, 0xC0, 0x01, // add ax, 1
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 0x1234_0000);
    assert!(e.eflags & ZF != 0 && e.eflags & CF != 0);
}

#[test]
fn high_byte_registers() {
    // mov eax,0; mov ah,0x12; mov al,0x34
    let e = run_ok(&[0x31, 0xC0, 0xB4, 0x12, 0xB0, 0x34, 0xF4]);
    assert_eq!(e.regs[EAX], 0x1234);
}

#[test]
fn imul_three_operand() {
    // mov eax,-3; imul eax,eax,5
    let e = run_ok(&[0xB8, 0xFD, 0xFF, 0xFF, 0xFF, 0x6B, 0xC0, 5, 0xF4]);
    assert_eq!(e.regs[EAX] as i32, -15);
}

#[test]
fn adc_and_sbb_chain_carry() {
    // 64-bit add: (1:ffffffff) + (0:1) -> (2:0)
    let e = run_ok(&[
        0xB8, 0xFF, 0xFF, 0xFF, 0xFF, // mov eax, 0xffffffff
        0xBA, 1, 0, 0, 0, // mov edx, 1
        0x83, 0xC0, 1, // add eax, 1
        0x83, 0xD2, 0, // adc edx, 0
        0xF4,
    ]);
    assert_eq!((e.regs[EDX], e.regs[EAX]), (2, 0));
}

#[test]
fn inc_preserves_carry() {
    // stc; inc eax; -> CF still set
    let e = run_ok(&[0xF9, 0x40, 0xF4]);
    assert!(e.eflags & CF != 0);
}

#[test]
fn fs_segment_reads_teb() {
    let mut e = Emu::new();
    e.mem.map(CODE, 0x10000);
    e.fs_base = 0x2000;
    e.mem.write(0x2018, 4, 0xCAFE).unwrap();
    e.mem
        .write_bytes(CODE, &[0x64, 0xA1, 0x18, 0, 0, 0, 0xF4])
        .unwrap();
    e.eip = CODE;
    assert_eq!(e.run(10), Stop::Halt);
    assert_eq!(e.regs[EAX], 0xCAFE);
}

#[test]
fn leave_and_frame() {
    // push ebp; mov ebp,esp; sub esp,16; leave
    let e = run_ok(&[0x55, 0x89, 0xE5, 0x83, 0xEC, 0x10, 0xC9, 0xF4]);
    assert_eq!(e.regs[ESP], 0x10000);
}

#[test]
fn indirect_call_through_memory() {
    let mut e = Emu::new();
    e.mem.map(CODE, 0x10000);
    e.mem.write(0x2000, 4, CODE + 8).unwrap();
    // call [0x2000]; hlt; (pad) ; mov eax, 77; ret
    e.mem
        .write_bytes(
            CODE,
            &[
                0xFF, 0x15, 0, 0x20, 0, 0, 0xF4, 0x90, 0xB8, 77, 0, 0, 0, 0xC3,
            ],
        )
        .unwrap();
    e.eip = CODE;
    e.regs[ESP] = 0x10000;
    assert_eq!(e.run(100), Stop::Halt);
    assert_eq!(e.regs[EAX], 77);
}

#[test]
fn bit_test_family_with_register_offsets() {
    let e = run_ok(&[
        0xB8, 0, 0, 0, 0, // mov eax, 0
        0xBB, 5, 0, 0, 0, // mov ebx, 5
        0x0F, 0xAB, 0xD8, // bts eax, ebx   -> eax = 0x20, CF = 0
        0x9C, 0x59, // pushfd; pop ecx  (capture flags)
        0x0F, 0xA3, 0xD8, // bt eax, ebx    -> CF = 1
        0x9C, 0x5A, // pushfd; pop edx
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 0x20);
    assert_eq!(e.regs[ECX] & CF, 0, "bit was clear");
    assert_eq!(e.regs[EDX] & CF, CF, "bit is now set");
}

#[test]
fn bit_test_on_memory_can_reach_past_the_operand() {
    // bts [ebx], ecx with ecx = 35 touches bit 3 of the *second* dword.
    let e = run_ok(&[
        0xBB, 0, 0x20, 0, 0, // mov ebx, 0x2000
        0xB9, 35, 0, 0, 0, // mov ecx, 35
        0x0F, 0xAB, 0x0B, // bts [ebx], ecx
        0xF4,
    ]);
    assert_eq!(e.mem.read(0x2000, 4).unwrap(), 0);
    assert_eq!(e.mem.read(0x2004, 4).unwrap(), 8);
}

#[test]
fn bit_test_immediates_reset_and_complement() {
    let e = run_ok(&[
        0xB8, 0xFF, 0, 0, 0, // mov eax, 0xff
        0x0F, 0xBA, 0xF0, 3, // btr eax, 3 -> 0xf7, CF = 1
        0x0F, 0xBA, 0xF8, 0, // btc eax, 0 -> 0xf6
        0x0F, 0xBA, 0xE8, 8, // bts eax, 8 -> 0x1f6
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 0x1F6);
}

#[test]
fn bsf_bsr_and_zero_source() {
    let e = run_ok(&[
        0xB8, 0x50, 0, 0, 0, // mov eax, 0x50
        0xB9, 0x77, 0, 0, 0, // mov ecx, 0x77 (must survive a zero source)
        0x0F, 0xBC, 0xD0, // bsf edx, eax -> 4
        0x0F, 0xBD, 0xD8, // bsr ebx, eax -> 6
        0x31, 0xF6, // xor esi, esi
        0x0F, 0xBC, 0xCE, // bsf ecx, esi (source 0): ZF set, ecx untouched
        0xF4,
    ]);
    assert_eq!((e.regs[EDX], e.regs[EBX]), (4, 6));
    assert_eq!(e.regs[ECX], 0x77);
    assert!(e.eflags & ZF != 0);
}

#[test]
fn shld_and_shrd() {
    let e = run_ok(&[
        0xB8, 0x78, 0x56, 0x34, 0x12, // eax = 0x12345678
        0xBB, 0xF0, 0xDE, 0xBC, 0x9A, // ebx = 0x9abcdef0
        0x0F, 0xA4, 0xD8, 8, // shld eax, ebx, 8
        0xB9, 0x78, 0x56, 0x34, 0x12, // ecx = 0x12345678
        0x0F, 0xAC, 0xD9, 8, // shrd ecx, ebx, 8
        0xF4,
    ]);
    assert_eq!(e.regs[EAX], 0x3456_789A);
    assert_eq!(e.regs[ECX], 0xF012_3456);
}

#[test]
fn cmpxchg_success_and_failure_with_lock_prefix() {
    let mut e = Emu::new();
    e.mem.map(CODE, 0x10000);
    e.mem.write(0x2000, 4, 5).unwrap();
    e.mem
        .write_bytes(
            CODE,
            &[
                0xB8, 5, 0, 0, 0, // eax = 5 (matches)
                0xBB, 9, 0, 0, 0, // ebx = 9
                0xF0, 0x0F, 0xB1, 0x1D, 0, 0x20, 0, 0, // lock cmpxchg [0x2000], ebx
                0x9C, 0x59, // flags -> ecx
                0xB8, 7, 0, 0, 0, // eax = 7 (no longer matches 9)
                0x0F, 0xB1, 0x1D, 0, 0x20, 0, 0, // cmpxchg [0x2000], ebx -> fails, eax = 9
                0x9C, 0x5A, // flags -> edx
                0xF4,
            ],
        )
        .unwrap();
    e.eip = CODE;
    e.regs[ESP] = 0x10000;
    assert_eq!(e.run(100), Stop::Halt);
    assert_eq!(
        e.mem.read(0x2000, 4).unwrap(),
        9,
        "exchange happened on the first, matching compare"
    );
    assert!(e.regs[ECX] & ZF != 0, "ZF set on success");
    assert_eq!(
        e.regs[EAX], 9,
        "failed compare loads the memory value into eax"
    );
    assert!(e.regs[EDX] & ZF == 0, "ZF clear on failure");
}

#[test]
fn xadd_and_multibyte_nop() {
    let e = run_ok(&[
        0xB8, 1, 0, 0, 0, 0xBB, 2, 0, 0, 0, // eax = 1, ebx = 2
        0x0F, 0xC1, 0xD8, // xadd eax, ebx -> eax = 3, ebx = 1
        0x0F, 0x1F, 0x44, 0x00, 0x00, // nopl 0(%eax,%eax,1): five bytes, no effect
        0xB9, 0x2A, 0, 0, 0, // mov ecx, 42 (proves the nop consumed exactly its bytes)
        0xF4,
    ]);
    assert_eq!((e.regs[EAX], e.regs[EBX], e.regs[ECX]), (3, 1, 42));
}

#[test]
fn segment_register_push_pop_and_mov() {
    let e = run_ok(&[
        0xB8, 0x78, 0x56, 0x34, 0x12, // eax = 0x12345678
        0x1E, 0x06, // push ds; push es
        0xB8, 0, 0, 0, 0, // clobber eax
        0x07, 0x1F, // pop es; pop ds  (what Miles does around its inline asm)
        0x8C, 0xDB, // mov ebx, ds
        0x8C, 0xC9, // mov ecx, cs
        0x0F, 0xA0, 0x5A, // push fs; pop edx
        0xF4,
    ]);
    assert_eq!(e.regs[ESP], 0x10000, "pushes and pops balance");
    assert_eq!((e.regs[EBX], e.regs[ECX], e.regs[EDX]), (0x23, 0x1B, 0x3B));
}

#[test]
fn mov_to_a_segment_register_is_harmless() {
    let e = run_ok(&[0x8C, 0xD8, 0x8E, 0xC0, 0x8E, 0xD8, 0xB8, 7, 0, 0, 0, 0xF4]); // mov eax,ds; mov es,ax; mov ds,ax
    assert_eq!(e.regs[EAX], 7);
}

#[test]
fn bswap() {
    let e = run_ok(&[0xB8, 0x78, 0x56, 0x34, 0x12, 0x0F, 0xC8, 0xF4]);
    assert_eq!(e.regs[EAX], 0x7856_3412);
}

#[test]
fn unmapped_read_reports_fault_address() {
    let (_, stop) = run(&[0xA1, 0, 0, 0, 0x90]);
    assert_eq!(stop, Stop::Fault(Fault(0x9000_0000)));
}

#[test]
fn unknown_opcode_reports_location() {
    let (_, stop) = run(&[0x0F, 0x0B]);
    assert!(matches!(stop, Stop::InvalidOpcode { eip: CODE, .. }));
}

#[test]
fn step_limit() {
    let (_, stop) = {
        let mut e = Emu::new();
        e.mem.map(CODE, 0x1000);
        e.mem.write_bytes(CODE, &[0xEB, 0xFE]).unwrap(); // jmp $
        e.eip = CODE;
        let s = e.run(50);
        (e, s)
    };
    assert_eq!(stop, Stop::StepLimit);
}
