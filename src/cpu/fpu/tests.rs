//! Tests of `fpu`.

use crate::cpu::{CF, EAX, Emu, Stop, ZF};

const CODE: u32 = 0x1000;
const A: u32 = 0x2000;
const B: u32 = 0x2100;
const OUT: u32 = 0x2200;

fn abs(op: &[u8], addr: u32) -> Vec<u8> {
    let mut v = op.to_vec();
    v.extend_from_slice(&addr.to_le_bytes());
    v
}

/// Assemble `chunks`, append hlt, run.
fn run(setup: impl FnOnce(&mut Emu), chunks: &[Vec<u8>]) -> Emu {
    let mut e = Emu::new();
    e.mem.map(CODE, 0x10000);
    setup(&mut e);
    let mut code: Vec<u8> = chunks.concat();
    code.push(0xF4);
    e.mem.write_bytes(CODE, &code).unwrap();
    e.eip = CODE;
    assert_eq!(e.run(10_000), Stop::Halt);
    e
}

fn write_f64(e: &mut Emu, addr: u32, v: f64) {
    let b = v.to_bits();
    e.mem.write(addr, 4, b as u32).unwrap();
    e.mem.write(addr + 4, 4, (b >> 32) as u32).unwrap();
}

fn read_f64(e: &Emu, addr: u32) -> f64 {
    let lo = e.mem.read(addr, 4).unwrap() as u64;
    let hi = e.mem.read(addr + 4, 4).unwrap() as u64;
    f64::from_bits((hi << 32) | lo)
}

/// The x87 instructions the JIT compiles natively (64-bit fild/fistp, fist/fistp m32 under
/// every rounding mode, fsqrt, fldcw, integer-operand arithmetic) give the same results
/// compiled as interpreted: a loop runs them until the block is hot.
#[test]
fn compiled_x87_matches_the_interpreter() {
    const I: u32 = 0x2300;
    const J: u32 = 0x2310;
    let body: Vec<u8> = [
        abs(&[0xD9, 0x2D], B),       // fldcw [B]
        abs(&[0xDD, 0x05], A),       // fld qword [A]
        abs(&[0xDB, 0x15], OUT),     // fist dword
        vec![0xD9, 0xC0],            // fld st0
        abs(&[0xDF, 0x3D], OUT + 8), // fistp qword
        vec![0xD9, 0xC0],
        abs(&[0xDB, 0x1D], OUT + 16), // fistp dword
        abs(&[0xDF, 0x2D], I),        // fild qword
        vec![0xD8, 0xC1],             // fadd st0, st1
        abs(&[0xDD, 0x1D], OUT + 24), // fstp qword
        abs(&[0xDD, 0x05], A),
        vec![0xD9, 0xE1, 0xD9, 0xFA], // fabs, fsqrt
        abs(&[0xDD, 0x1D], OUT + 32),
        abs(&[0xDA, 0x35], J), // fidiv dword
        abs(&[0xDA, 0x0D], J), // fimul dword
        abs(&[0xDA, 0x25], J), // fisub dword
        abs(&[0xDD, 0x1D], OUT + 40),
    ]
    .concat();
    let mut code = body.clone();
    code.push(0x49); // dec ecx
    code.extend([0x75, (-(code.len() as i32 + 2)) as u8]); // jnz to the top
    for v in [
        2.5,
        -2.5,
        3.7,
        -3.7,
        1e20,
        -1e20,
        9.3e18,
        -9.3e18,
        f64::NAN,
        0.0,
        -0.4,
    ] {
        for cw in [0x037Fu32, 0x077F, 0x0B7F, 0x0F7F, 0x007F, 0x0C7F, 0x027F] {
            let go = |jit: bool| {
                let e = run(
                    |e| {
                        e.jit.enabled &= jit;
                        e.jit.stats = true;
                        e.regs[crate::cpu::ECX] = 40;
                        write_f64(e, A, v);
                        e.mem.write(B, 2, cw).unwrap();
                        e.mem.write(I, 4, 0x8000_0001).unwrap();
                        e.mem.write(I + 4, 4, 0xFFFF_FFF0).unwrap();
                        e.mem.write(J, 4, (-7i32) as u32).unwrap();
                    },
                    &[code.clone()],
                );
                if e.jit.enabled {
                    assert!(e.jit.compiled > 0, "the loop was compiled");
                    assert!(
                        e.jit.helper_kinds.is_empty(),
                        "natively: {:?}",
                        e.jit.helper_kinds
                    );
                }
                e.mem.read_bytes(OUT, 48).unwrap()
            };
            assert_eq!(go(true), go(false), "value {v} control word {cw:#x}");
        }
    }
}

#[test]
fn one_plus_one_is_two() {
    let e = run(
        |_| {},
        &[
            vec![0xD9, 0xE8, 0xD9, 0xE8, 0xDE, 0xC1],
            abs(&[0xDB, 0x1D], OUT),
        ], // fld1 fld1 faddp fistp
    );
    assert_eq!(e.mem.read(OUT, 4).unwrap(), 2);
}

#[test]
fn float32_multiply_stored_as_float64() {
    let e = run(
        |e| {
            e.mem.write(A, 4, 1.5f32.to_bits()).unwrap();
            e.mem.write(B, 4, 2.0f32.to_bits()).unwrap();
        },
        &[
            abs(&[0xD9, 0x05], A),
            abs(&[0xD8, 0x0D], B),
            abs(&[0xDD, 0x1D], OUT),
        ],
    );
    assert_eq!(read_f64(&e, OUT), 3.0);
}

#[test]
fn sub_and_div_register_forms_have_correct_operand_order() {
    // st1 = 10, st0 = 3; fsubp st1,st0 -> 7
    let e = run(
        |e| {
            e.mem.write(A, 4, 10).unwrap();
            e.mem.write(B, 4, 3).unwrap();
        },
        &[
            abs(&[0xDB, 0x05], A), // fild [A]
            abs(&[0xDB, 0x05], B), // fild [B]
            vec![0xDE, 0xE9],      // fsubp st1, st0
            abs(&[0xDB, 0x1D], OUT),
        ],
    );
    assert_eq!(e.mem.read(OUT, 4).unwrap(), 7);

    // 12 / 4 via fdivp st1, st0 (DE F9)
    let e = run(
        |e| {
            e.mem.write(A, 4, 12).unwrap();
            e.mem.write(B, 4, 4).unwrap();
        },
        &[
            abs(&[0xDB, 0x05], A),
            abs(&[0xDB, 0x05], B),
            vec![0xDE, 0xF9],
            abs(&[0xDB, 0x1D], OUT),
        ],
    );
    assert_eq!(e.mem.read(OUT, 4).unwrap(), 3);
}

#[test]
fn fcomip_sets_cpu_flags() {
    // fld1; fldz; fcomip st0,st1 (0 < 1 -> CF); fstp st0
    let e = run(
        |_| {},
        &[vec![0xD9, 0xE8, 0xD9, 0xEE, 0xDF, 0xF1, 0xDD, 0xD8]],
    );
    assert!(e.eflags & CF != 0);
    assert!(e.eflags & ZF == 0);
    // equal
    let e = run(
        |_| {},
        &[vec![0xD9, 0xE8, 0xD9, 0xE8, 0xDF, 0xF1, 0xDD, 0xD8]],
    );
    assert!(e.eflags & ZF != 0 && e.eflags & CF == 0);
}

#[test]
fn fnstsw_sahf_branch_idiom() {
    // fld1 (st1=1); fldz (st0=0); fcompp -> st0 < st1 -> C0; fnstsw ax; sahf -> CF
    let e = run(
        |_| {},
        &[vec![0xD9, 0xE8, 0xD9, 0xEE, 0xDE, 0xD9, 0xDF, 0xE0, 0x9E]],
    );
    assert!(e.eflags & CF != 0);
    assert_eq!(e.regs[EAX] & 0x4500, 0x0100);
}

#[test]
fn sqrt_of_integer() {
    let e = run(
        |e| e.mem.write(A, 4, 16).unwrap(),
        &[
            abs(&[0xDB, 0x05], A),
            vec![0xD9, 0xFA],
            abs(&[0xDB, 0x1D], OUT),
        ],
    );
    assert_eq!(e.mem.read(OUT, 4).unwrap(), 4);
}

#[test]
fn fist_honours_rounding_control() {
    let prog = |cw: u16| {
        run(
            |e| {
                write_f64(e, A, 2.7);
                e.mem.write(B, 2, cw as u32).unwrap();
            },
            &[
                abs(&[0xD9, 0x2D], B),
                abs(&[0xDD, 0x05], A),
                abs(&[0xDB, 0x1D], OUT),
            ],
        )
    };
    assert_eq!(
        prog(0x037F).mem.read(OUT, 4).unwrap(),
        3,
        "round to nearest"
    );
    assert_eq!(prog(0x0F7F).mem.read(OUT, 4).unwrap(), 2, "truncate");
}

#[test]
fn precision_control_rounds_arithmetic_to_single() {
    // fldcw [B]; fld1; fld qword [A] (3.0); fdivp st1 (1/3); fstp qword [OUT]
    let third = |cw: u16| {
        let e = run(
            |e| {
                write_f64(e, A, 3.0);
                e.mem.write(B, 2, cw as u32).unwrap();
            },
            &[
                abs(&[0xD9, 0x2D], B),
                vec![0xD9, 0xE8],
                abs(&[0xDD, 0x05], A),
                vec![0xDE, 0xF9],
                abs(&[0xDD, 0x1D], OUT),
            ],
        );
        read_f64(&e, OUT)
    };
    assert_eq!(
        third(0x007F),
        (1.0f32 / 3.0) as f64,
        "single precision (Direct3D's)"
    );
    assert_eq!(third(0x027F), 1.0 / 3.0, "double precision");
}

#[test]
fn nearest_rounds_ties_to_even() {
    let e = run(
        |e| write_f64(e, A, 2.5),
        &[abs(&[0xDD, 0x05], A), abs(&[0xDB, 0x1D], OUT)],
    );
    assert_eq!(e.mem.read(OUT, 4).unwrap(), 2);
}

#[test]
fn out_of_range_gives_integer_indefinite() {
    let e = run(
        |e| write_f64(e, A, 1e20),
        &[abs(&[0xDD, 0x05], A), abs(&[0xDB, 0x1D], OUT)],
    );
    assert_eq!(e.mem.read(OUT, 4).unwrap(), 0x8000_0000);
}

#[test]
fn extended_precision_roundtrip() {
    for v in [std::f64::consts::PI, -0.5, 1e300, 1e-300, 0.0, 123456789.0] {
        let e = run(
            |e| write_f64(e, A, v),
            &[
                abs(&[0xDD, 0x05], A),   // fld qword [A]
                abs(&[0xDB, 0x3D], B),   // fstp tbyte [B]
                abs(&[0xDB, 0x2D], B),   // fld tbyte [B]
                abs(&[0xDD, 0x1D], OUT), // fstp qword [OUT]
            ],
        );
        assert_eq!(read_f64(&e, OUT), v, "{v}");
    }
}

#[test]
fn constants_and_trig() {
    // fldpi; fcos -> -1
    let e = run(
        |_| {},
        &[vec![0xD9, 0xEB, 0xD9, 0xFF], abs(&[0xDD, 0x1D], OUT)],
    );
    assert!((read_f64(&e, OUT) + 1.0).abs() < 1e-12);
    // fld1 fld1 fpatan -> pi/4
    let e = run(
        |_| {},
        &[
            vec![0xD9, 0xE8, 0xD9, 0xE8, 0xD9, 0xF3],
            abs(&[0xDD, 0x1D], OUT),
        ],
    );
    assert!((read_f64(&e, OUT) - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
}

#[test]
fn fxam_classifies_st0() {
    use super::{C2, C3, CC_MASK};
    // fld1; fchs; fxam; fnstsw ax  -> normal (C2) and negative (C1)
    let e = run(
        |_| {},
        &[vec![0xD9, 0xE8, 0xD9, 0xE0, 0xD9, 0xE5, 0xDF, 0xE0]],
    );
    assert_eq!(e.regs[EAX] as u16 & CC_MASK, C2 | 0x0200);
    // fldz; fxam -> zero (C3), positive
    let e = run(|_| {}, &[vec![0xD9, 0xEE, 0xD9, 0xE5, 0xDF, 0xE0]]);
    assert_eq!(e.regs[EAX] as u16 & CC_MASK, C3);
}

#[test]
fn fxch_fchs_fabs() {
    // fld1; fldz; fxch -> st0 = 1; fchs -> -1; fabs -> 1 ; store
    let e = run(
        |_| {},
        &[
            vec![0xD9, 0xE8, 0xD9, 0xEE, 0xD9, 0xC9, 0xD9, 0xE0, 0xD9, 0xE1],
            abs(&[0xDD, 0x1D], OUT),
        ],
    );
    assert_eq!(read_f64(&e, OUT), 1.0);
}

#[test]
fn fild_int64_and_fistp_int64() {
    let e = run(
        |e| {
            e.mem.write(A, 4, 0).unwrap();
            e.mem.write(A + 4, 4, 1).unwrap(); // 1 << 32
        },
        &[abs(&[0xDF, 0x2D], A), abs(&[0xDF, 0x3D], OUT)],
    );
    assert_eq!(e.mem.read(OUT + 4, 4).unwrap(), 1);
    assert_eq!(e.mem.read(OUT, 4).unwrap(), 0);
}

#[test]
fn fwait_before_fnstcw() {
    // fwait; fnstcw [OUT]  (the sequence compilers emit for fstcw)
    let e = run(|_| {}, &[vec![0x9B], abs(&[0xD9, 0x3D], OUT)]);
    assert_eq!(e.mem.read(OUT, 2).unwrap(), 0x037F, "default control word");
}

#[test]
fn stack_wraps_after_push_pop() {
    let e = run(|_| {}, &[[0xD9, 0xE8].repeat(3), [0xDD, 0xD8].repeat(3)]);
    assert_eq!(
        e.fpu.status_word() >> 11,
        0,
        "balanced push/pop returns TOP to 0"
    );
}
