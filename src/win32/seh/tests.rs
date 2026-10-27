//! Tests of `seh`.

use super::*;
use crate::cpu::{Emu, Stop};
use crate::loader;
use crate::memory::Fault;
use crate::tests::util::{CODE_VA, build_pe};
use std::collections::HashMap;

const DATA: u32 = 0x300; // code-section offset of the test's scratch variables

/// A tiny assembler with absolute label addresses.
#[derive(Default)]
struct A {
    b: Vec<u8>,
    labels: HashMap<&'static str, u32>,
    fix: Vec<(usize, &'static str)>,
}

impl A {
    fn e(&mut self, bytes: &[u8]) -> &mut Self {
        self.b.extend_from_slice(bytes);
        self
    }
    fn label(&mut self, n: &'static str) -> &mut Self {
        self.labels.insert(n, self.b.len() as u32);
        self
    }
    /// A 32-bit immediate holding the virtual address of `label`.
    fn va(&mut self, n: &'static str) -> &mut Self {
        self.fix.push((self.b.len(), n));
        self.b.extend_from_slice(&[0; 4]);
        self
    }
    fn push_va(&mut self, n: &'static str) -> &mut Self {
        self.e(&[0x68]).va(n)
    }
    fn call(&mut self, slot: u32) -> &mut Self {
        self.e(&[0xFF, 0x15]);
        self.b.extend_from_slice(&slot.to_le_bytes());
        self
    }
    /// mov [DATA + slot*4], eax
    fn store(&mut self, slot: u32) -> &mut Self {
        self.e(&[0xA3]);
        self.b
            .extend_from_slice(&(CODE_VA + DATA + 4 * slot).to_le_bytes());
        self
    }
    fn finish(mut self) -> Vec<u8> {
        for (pos, n) in std::mem::take(&mut self.fix) {
            let v = CODE_VA + self.labels[n];
            self.b[pos..pos + 4].copy_from_slice(&v.to_le_bytes());
        }
        assert!(self.b.len() <= DATA as usize, "code overlaps the data area");
        self.b.resize(DATA as usize + 0x100, 0);
        self.b
    }
}

fn data(emu: &Emu, slot: u32) -> u32 {
    emu.mem.read(CODE_VA + DATA + 4 * slot, 4).unwrap()
}

const K32: &[&str] = &[
    "RaiseException",
    "RtlUnwind",
    "ExitProcess",
    "SetUnhandledExceptionFilter",
];

/// The handler that records the exception into DATA slots 0..4 and resumes after the
/// faulting instruction (`skip` bytes), by editing the context's EIP.
fn recording_skip_handler(a: &mut A, name: &'static str, skip: u8) {
    a.label(name);
    a.e(&[0x8B, 0x4C, 0x24, 0x04]); // mov ecx,[esp+4]    ; EXCEPTION_RECORD*
    a.e(&[0x8B, 0x01]).store(0); // code
    a.e(&[0x8B, 0x41, 0x04]).store(1); // flags
    a.e(&[0x8B, 0x41, 0x0C]).store(2); // exception address
    a.e(&[0x8B, 0x41, 0x14]).store(3); // ExceptionInformation[0]
    a.e(&[0x8B, 0x41, 0x18]).store(4); // ExceptionInformation[1]
    a.e(&[0x8B, 0x44, 0x24, 0x0C]); // mov eax,[esp+12]   ; CONTEXT*
    a.e(&[0x83, 0x80, 0xB8, 0, 0, 0, skip]); // add dword [eax+0xB8], skip   ; context EIP
    a.e(&[0x31, 0xC0, 0xC3]); // xor eax,eax (ContinueExecution); ret
}

/// Link frame -> handler, run `setup` + `fault`, unlink, ExitProcess(ebx = 0x1234).
fn fault_program(setup: &[u8], fault: &[u8], extra_inner_handler: bool) -> Vec<u8> {
    build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = A::default();
        a.push_va("outer")
            .e(&[0x64, 0xFF, 0x35, 0, 0, 0, 0])
            .e(&[0x64, 0x89, 0x25, 0, 0, 0, 0]); // outer frame
        if extra_inner_handler {
            a.push_va("inner")
                .e(&[0x64, 0xFF, 0x35, 0, 0, 0, 0])
                .e(&[0x64, 0x89, 0x25, 0, 0, 0, 0]); // inner frame
        }
        a.e(setup);
        a.label("fault").e(fault);
        a.e(&[0xBB, 0x34, 0x12, 0, 0]); // mov ebx, 0x1234  (only reached if resumed)
        if extra_inner_handler {
            a.e(&[0x64, 0x8F, 0x05, 0, 0, 0, 0, 0x83, 0xC4, 4]); // pop fs:[0]; add esp,4
        }
        a.e(&[0x64, 0x8F, 0x05, 0, 0, 0, 0, 0x83, 0xC4, 4]);
        a.e(&[0x53]).call(l.iat_va(0, 2)); // ExitProcess(ebx)
        recording_skip_handler(&mut a, "outer", fault.len() as u8);
        if extra_inner_handler {
            // Inner handler: count the call in slot 6 and say "not mine".
            a.label("inner").e(&[0xFF, 0x05]);
            a.b.extend_from_slice(&(CODE_VA + DATA + 24).to_le_bytes());
            a.e(&[0xB8, 1, 0, 0, 0, 0xC3]); // mov eax,1 (ContinueSearch); ret
        }
        a.finish()
    })
}

fn run(image: &[u8]) -> (Emu, Stop) {
    let mut emu = Emu::new();
    loader::load(&mut emu, image).unwrap();
    let stop = emu.run(100_000);
    (emu, stop)
}

fn fault_va(image: &[u8], fault: &[u8]) -> u32 {
    let start = image
        .windows(fault.len() + 5)
        .position(|w| w[..fault.len()] == *fault && w[fault.len()] == 0xBB)
        .expect("fault bytes");
    // The image is the PE file; code begins at file offset 0x200 and maps to CODE_VA.
    CODE_VA + (start as u32 - 0x200)
}

#[test]
fn access_violation_is_delivered_and_a_handler_can_resume_past_it() {
    let fault = [0x8B, 0x08]; // mov ecx,[eax] with eax = 0
    let image = fault_program(&[0x31, 0xC0], &fault, false);
    let (emu, stop) = run(&image);
    assert_eq!(
        stop,
        Stop::Exit(0x1234),
        "execution resumed after the faulting instruction"
    );
    assert_eq!(data(&emu, 0), STATUS_ACCESS_VIOLATION);
    assert_eq!(data(&emu, 1), 0, "continuable");
    assert_eq!(
        data(&emu, 2),
        fault_va(&image, &fault),
        "ExceptionAddress is the faulting instruction"
    );
    assert_eq!(
        (data(&emu, 3), data(&emu, 4)),
        (0, 0),
        "a read of address 0"
    );
    assert_eq!(
        emu.mem.read(emu.fs_base, 4).unwrap(),
        END_OF_CHAIN,
        "the chain is back to empty"
    );
    assert!(emu.seh.is_empty(), "no dispatch state left over");
}

#[test]
fn writes_are_reported_as_writes_with_the_address() {
    let fault = [0x89, 0x08]; // mov [eax],ecx with eax = 0x1234
    let image = fault_program(&[0xB8, 0x34, 0x12, 0, 0], &fault, false);
    let (emu, stop) = run(&image);
    assert_eq!(stop, Stop::Exit(0x1234));
    assert_eq!(
        (data(&emu, 3), data(&emu, 4)),
        (1, 0x1234),
        "information[0] = write, [1] = the address"
    );
}

#[test]
fn integer_divide_by_zero() {
    let image = fault_program(
        &[0xB8, 5, 0, 0, 0, 0x31, 0xD2, 0x31, 0xC9],
        &[0xF7, 0xF1],
        false,
    ); // div ecx, ecx = 0
    let (emu, stop) = run(&image);
    assert_eq!(stop, Stop::Exit(0x1234));
    assert_eq!(data(&emu, 0), STATUS_INTEGER_DIVIDE_BY_ZERO);
}

#[test]
fn illegal_instruction() {
    let image = fault_program(&[], &[0x0F, 0x0B], false); // ud2
    let (emu, stop) = run(&image);
    assert_eq!(stop, Stop::Exit(0x1234));
    assert_eq!(data(&emu, 0), STATUS_ILLEGAL_INSTRUCTION);
}

#[test]
fn handlers_are_tried_innermost_first_and_continue_search_moves_outward() {
    let image = fault_program(&[0x31, 0xC0], &[0x8B, 0x08], true);
    let (emu, stop) = run(&image);
    assert_eq!(stop, Stop::Exit(0x1234));
    assert_eq!(
        data(&emu, 6),
        1,
        "the inner handler ran once, said ContinueSearch"
    );
    assert_eq!(
        data(&emu, 0),
        STATUS_ACCESS_VIOLATION,
        "and the outer one then handled it"
    );
}

#[test]
fn with_no_handler_the_original_stop_is_reported_unchanged() {
    let image = build_pe(&[], |_| vec![0x31, 0xC0, 0x8B, 0x08, 0xC3]); // mov ecx,[0]
    let (emu, stop) = run(&image);
    assert_eq!(stop, Stop::Fault(Fault(0)));
    assert_eq!(
        emu.eip,
        CODE_VA + 2,
        "eip is back on the faulting instruction"
    );
    assert!(emu.seh.is_empty());
}

/// C++-style throw/catch: RaiseException with a frame whose handler calls RtlUnwind and
/// then jumps into its catch block with the stack reset. A newer frame in between has a
/// termination handler that must run during the unwind.
fn throw_catch_program() -> Vec<u8> {
    build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = A::default();
        // outer (catching) frame
        a.push_va("catcher")
            .e(&[0x64, 0xFF, 0x35, 0, 0, 0, 0])
            .e(&[0x64, 0x89, 0x25, 0, 0, 0, 0]);
        a.e(&[0x89, 0x25]); // mov [DATA+0x20], esp   ; remember the frame address
        a.b.extend_from_slice(&(CODE_VA + DATA + 0x20).to_le_bytes());
        // inner frame with a cleanup handler
        a.push_va("cleanup")
            .e(&[0x64, 0xFF, 0x35, 0, 0, 0, 0])
            .e(&[0x64, 0x89, 0x25, 0, 0, 0, 0]);
        // throw: RaiseException(0xE06D7363, NONCONTINUABLE, 0, NULL)
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 1])
            .e(&[0x68])
            .b
            .extend_from_slice(&0xE06D_7363u32.to_le_bytes());
        a.call(l.iat_va(0, 0));
        // not caught: exit with a recognisable failure code
        a.e(&[0x68, 0xAD, 0x0B, 0, 0]).call(l.iat_va(0, 2));

        a.label("catch");
        a.e(&[0x8B, 0x25]); // mov esp, [DATA+0x20]   ; the stack as the catching frame had it
        a.b.extend_from_slice(&(CODE_VA + DATA + 0x20).to_le_bytes());
        a.e(&[0x64, 0x8F, 0x05, 0, 0, 0, 0, 0x83, 0xC4, 4]); // pop fs:[0]; add esp,4
        a.e(&[0x6A, 0x55]).call(l.iat_va(0, 2)); // ExitProcess(0x55)

        // cleanup(rec, frame, ctx, dctx): during an unwind, count it; always ContinueSearch
        a.label("cleanup");
        a.e(&[0x8B, 0x44, 0x24, 0x04, 0xF7, 0x40, 0x04, 2, 0, 0, 0]); // mov eax,[esp+4]; test [eax+4], UNWINDING
        a.e(&[0x74, 0x06]); // jz skip
        a.e(&[0xFF, 0x05]);
        a.b.extend_from_slice(&(CODE_VA + DATA + 24).to_le_bytes()); // inc [DATA+24]
        a.e(&[0xB8, 1, 0, 0, 0, 0xC3]); // skip: mov eax,1; ret

        // catcher(rec, frame, ctx, dctx): RtlUnwind(frame, catch, rec, 0); jmp catch
        a.label("catcher");
        a.e(&[0x6A, 0]); // retval
        a.e(&[0xFF, 0x74, 0x24, 0x08]); // rec    (arg1, shifted by the push)
        a.push_va("catch");
        a.e(&[0xFF, 0x74, 0x24, 0x14]); // frame  (arg2, shifted by three pushes)
        a.call(l.iat_va(0, 1)); // RtlUnwind
        a.e(&[0xB8]).va("catch");
        a.e(&[0xFF, 0xE0]); // jmp eax
        a.finish()
    })
}

#[test]
fn throw_unwind_and_catch_runs_cleanup_handlers_and_lands_in_the_catch_block() {
    let (emu, stop) = run(&throw_catch_program());
    assert_eq!(
        stop,
        Stop::Exit(0x55),
        "caught (0xbad would mean it was not)"
    );
    assert_eq!(
        data(&emu, 6),
        1,
        "the inner cleanup handler ran exactly once, during the unwind"
    );
    assert_eq!(emu.mem.read(emu.fs_base, 4).unwrap(), END_OF_CHAIN);
}

fn raise_program(with_filter: Option<&'static [u8]>) -> Vec<u8> {
    build_pe(&[("KERNEL32.dll", K32)], |l| {
        let mut a = A::default();
        if with_filter.is_some() {
            a.push_va("filter").call(l.iat_va(0, 3)); // SetUnhandledExceptionFilter
        }
        a.e(&[0x6A, 0, 0x6A, 0, 0x6A, 0, 0x68, 0x34, 0x12, 0, 0])
            .call(l.iat_va(0, 0)); // RaiseException(0x1234, 0, 0, 0)
        a.e(&[0x6A, 77]).call(l.iat_va(0, 2)); // ExitProcess(77): reached only if execution continues
        a.label("filter");
        if let Some(code) = with_filter {
            a.e(code);
        }
        a.finish()
    })
}

#[test]
fn unhandled_software_exception_reports_its_code_and_address() {
    let (_, stop) = run(&raise_program(None));
    assert!(
        matches!(stop, Stop::Exception { code: 0x1234, .. }),
        "{stop:?}"
    );
}

#[test]
fn unhandled_exception_filter_can_continue_execution() {
    let (_, stop) = run(&raise_program(Some(&[
        0xB8, 0xFF, 0xFF, 0xFF, 0xFF, 0xC2, 4, 0,
    ]))); // return -1
    assert_eq!(
        stop,
        Stop::Exit(77),
        "CONTINUE_EXECUTION resumed after RaiseException"
    );
}

#[test]
fn unhandled_exception_filter_can_terminate_with_the_exception_code() {
    let (_, stop) = run(&raise_program(Some(&[0xB8, 1, 0, 0, 0, 0xC2, 4, 0]))); // return EXECUTE_HANDLER
    assert_eq!(stop, Stop::Exit(0x1234));
}

#[test]
fn unhandled_exception_filter_returning_search_falls_through_to_the_default() {
    let (_, stop) = run(&raise_program(Some(&[0x31, 0xC0, 0xC2, 4, 0])));
    assert!(
        matches!(stop, Stop::Exception { code: 0x1234, .. }),
        "{stop:?}"
    );
}
