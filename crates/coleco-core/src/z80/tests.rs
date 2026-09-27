// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! Z80 unit tests.
//!
//! These are not a substitute for ZEXALL, which is the real gate. They exist so
//! that a gross decode error is found against six bytes of hand-written code
//! rather than against a 64 KiB exerciser that prints nothing when it is stuck.

use super::{flag, Bus, A, B, C, D, E, H, L, Z80};

/// Flat 64 KiB with a log of I/O, which is all a decode test needs.
struct Flat {
    mem: Vec<u8>,
    out: Vec<(u16, u8)>,
    input_value: u8,
}

impl Flat {
    fn new(code: &[u8]) -> Self {
        let mut mem = vec![0u8; 0x1_0000];
        mem[..code.len()].copy_from_slice(code);
        Flat {
            mem,
            out: Vec::new(),
            input_value: 0,
        }
    }
}

impl Bus for Flat {
    fn read(&mut self, addr: u16) -> u8 {
        self.mem[addr as usize]
    }
    fn write(&mut self, addr: u16, val: u8) {
        self.mem[addr as usize] = val;
    }
    fn input(&mut self, _port: u16) -> u8 {
        self.input_value
    }
    fn output(&mut self, port: u16, val: u8) {
        self.out.push((port, val));
    }
}

fn run(code: &[u8], steps: usize) -> (Z80, Flat) {
    let mut bus = Flat::new(code);
    let mut cpu = Z80::new();
    cpu.reset();
    cpu.sp = 0xf000;
    for _ in 0..steps {
        cpu.step(&mut bus);
    }
    (cpu, bus)
}

#[test]
fn loads_and_arithmetic() {
    // LD A,$12 ; LD B,$34 ; ADD A,B ; LD ($8000),A
    let (cpu, bus) = run(&[0x3e, 0x12, 0x06, 0x34, 0x80, 0x32, 0x00, 0x80], 4);
    assert_eq!(cpu.a(), 0x46);
    assert_eq!(cpu.reg(B), 0x34);
    assert_eq!(bus.mem[0x8000], 0x46);
}

/// The carry and half-carry rules, and the sign/zero pair, on a case where all
/// four are decidable by hand.
#[test]
fn add_sets_carry_half_and_overflow() {
    // LD A,$FF ; ADD A,$01
    let (cpu, _) = run(&[0x3e, 0xff, 0xc6, 0x01], 2);
    assert_eq!(cpu.a(), 0x00);
    assert_ne!(cpu.f & flag::Z, 0, "result is zero");
    assert_ne!(cpu.f & flag::C, 0, "carried out");
    assert_ne!(cpu.f & flag::H, 0, "half-carried");
    assert_eq!(cpu.f & flag::N, 0, "N is clear after an add");

    // LD A,$7F ; ADD A,$01 : signed overflow, no carry.
    let (cpu, _) = run(&[0x3e, 0x7f, 0xc6, 0x01], 2);
    assert_eq!(cpu.a(), 0x80);
    assert_ne!(cpu.f & flag::PV, 0, "signed overflow");
    assert_eq!(cpu.f & flag::C, 0, "no carry out");
    assert_ne!(cpu.f & flag::S, 0, "result is negative");
}

/// CP takes its undocumented bits from the operand, not the result. Every other
/// ALU operation takes them from the result, so this is the one that would go
/// unnoticed without a test.
#[test]
fn cp_takes_undocumented_bits_from_the_operand() {
    // LD A,$00 ; CP $28   ($28 has bits 5 and 3 set)
    let (cpu, _) = run(&[0x3e, 0x00, 0xfe, 0x28], 2);
    assert_eq!(cpu.a(), 0x00, "CP does not write A");
    assert_eq!(cpu.f & flag::XY, 0x28, "X and Y come from the operand");
}

#[test]
fn sixteen_bit_pairs_and_stack() {
    // LD HL,$1234 ; PUSH HL ; POP BC
    let (cpu, _) = run(&[0x21, 0x34, 0x12, 0xe5, 0xc1], 3);
    assert_eq!(cpu.hl(), 0x1234);
    assert_eq!(cpu.bc(), 0x1234);
    assert_eq!(cpu.sp, 0xf000, "the stack is balanced again");
}

#[test]
fn shadow_registers_swap() {
    // LD A,$11 ; LD B,$22 ; EX AF,AF' ; EXX ; LD A,$33 ; LD B,$44 ; EX AF,AF' ; EXX
    let code = [
        0x3e, 0x11, 0x06, 0x22, 0x08, 0xd9, 0x3e, 0x33, 0x06, 0x44, 0x08, 0xd9,
    ];
    let (cpu, _) = run(&code, 8);
    assert_eq!(cpu.a(), 0x11, "the first A came back");
    assert_eq!(cpu.reg(B), 0x22, "the first B came back");
}

/// A conditional relative jump and the counted loop, which between them cover
/// the two ways the Z80 changes PC by a displacement.
#[test]
fn djnz_counts_down_and_jr_takes_a_condition() {
    // LD B,$05 ; LD A,$00 ; INC A ; DJNZ -3 ; halt-ish
    let (cpu, _) = run(&[0x06, 0x05, 0x3e, 0x00, 0x3c, 0x10, 0xfd, 0x76], 20);
    assert_eq!(cpu.a(), 5, "the loop body ran once per count");
    assert_eq!(cpu.reg(B), 0);
}

/// The index prefix rewrites H and L, and `(IX+d)` reaches memory with a signed
/// displacement.
#[test]
fn index_prefix_addresses_and_registers() {
    // LD IX,$8000 ; LD (IX+2),$AB ; LD A,(IX+2)
    let code = [
        0xdd, 0x21, 0x00, 0x80, 0xdd, 0x36, 0x02, 0xab, 0xdd, 0x7e, 0x02,
    ];
    let (cpu, bus) = run(&code, 3);
    assert_eq!(cpu.ix, 0x8000);
    assert_eq!(bus.mem[0x8002], 0xab);
    assert_eq!(cpu.a(), 0xab);

    // A negative displacement.
    let code = [0xdd, 0x21, 0x10, 0x80, 0xdd, 0x36, 0xfe, 0x5a];
    let (_, bus) = run(&code, 2);
    assert_eq!(bus.mem[0x800e], 0x5a);
}

/// Under DD, H and L become IXH and IXL unless the instruction reaches memory.
#[test]
fn index_prefix_renames_h_and_l() {
    // LD IX,$1234 ; LD A,IXH ; LD B,IXL
    let code = [0xdd, 0x21, 0x34, 0x12, 0xdd, 0x7c, 0xdd, 0x45];
    let (cpu, _) = run(&code, 3);
    assert_eq!(cpu.a(), 0x12, "IXH");
    assert_eq!(cpu.reg(B), 0x34, "IXL");
    assert_eq!(cpu.hl(), 0xffff, "the real HL was untouched");
}

#[test]
fn bit_set_and_reset() {
    // LD A,$00 ; SET 3,A ; BIT 3,A ; RES 3,A ; BIT 3,A
    let code = [0x3e, 0x00, 0xcb, 0xdf, 0xcb, 0x5f, 0xcb, 0x9f, 0xcb, 0x5f];
    let mut bus = Flat::new(&code);
    let mut cpu = Z80::new();
    cpu.reset();
    cpu.sp = 0xf000;
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x08);
    cpu.step(&mut bus);
    assert_eq!(cpu.f & flag::Z, 0, "the bit is set, so Z is clear");
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x00);
    cpu.step(&mut bus);
    assert_ne!(cpu.f & flag::Z, 0, "the bit is clear, so Z is set");
}

/// The block move, which touches four registers and two memory areas at once.
#[test]
fn ldir_copies_a_block() {
    let mut bus = Flat::new(&[0xed, 0xb0]); // LDIR
    for i in 0..8 {
        bus.mem[0x9000 + i] = (i as u8) + 1;
    }
    let mut cpu = Z80::new();
    cpu.reset();
    cpu.sp = 0xf000;
    cpu.set_hl_for_test(0x9000);
    cpu.set_de_for_test(0xa000);
    cpu.set_bc_for_test(8);
    for _ in 0..64 {
        cpu.step(&mut bus);
        if cpu.pc != 0 {
            break;
        }
    }
    assert_eq!(&bus.mem[0xa000..0xa008], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(cpu.bc(), 0, "the counter ran out");
    assert_eq!(cpu.hl(), 0x9008);
    assert_eq!(cpu.de(), 0xa008);
}

#[test]
fn call_and_ret() {
    // CALL $0100 ; (at $0100) LD A,$77 ; RET
    let mut code = vec![0xcd, 0x00, 0x01, 0x76];
    code.resize(0x100, 0);
    code.extend_from_slice(&[0x3e, 0x77, 0xc9]);
    let (cpu, _) = run(&code, 3);
    assert_eq!(cpu.a(), 0x77);
    assert_eq!(cpu.pc, 3, "returned past the CALL");
    assert_eq!(cpu.sp, 0xf000, "the stack is balanced");
}

#[test]
fn out_and_in_reach_the_io_space() {
    // LD A,$5A ; OUT ($7F),A ; IN A,($7E)
    let mut bus = Flat::new(&[0x3e, 0x5a, 0xd3, 0x7f, 0xdb, 0x7e]);
    bus.input_value = 0xc3;
    let mut cpu = Z80::new();
    cpu.reset();
    cpu.sp = 0xf000;
    for _ in 0..3 {
        cpu.step(&mut bus);
    }
    assert_eq!(bus.out, vec![(0x5a7f, 0x5a)], "the port's high byte is A");
    assert_eq!(cpu.a(), 0xc3);
}

/// An interrupt in mode 1 stacks the return address and vectors to $38, and the
/// instruction after `EI` is protected so a handler can return.
#[test]
fn interrupt_mode_1_vectors_and_ei_delays_one_instruction() {
    struct Irq {
        flat: Flat,
        raise: bool,
    }
    impl Bus for Irq {
        fn read(&mut self, a: u16) -> u8 {
            self.flat.read(a)
        }
        fn write(&mut self, a: u16, v: u8) {
            self.flat.write(a, v)
        }
        fn input(&mut self, p: u16) -> u8 {
            self.flat.input(p)
        }
        fn output(&mut self, p: u16, v: u8) {
            self.flat.output(p, v)
        }
        fn irq(&mut self) -> bool {
            self.raise
        }
    }

    // IM 1 ; EI ; NOP ; NOP
    let mut bus = Irq {
        flat: Flat::new(&[0xed, 0x56, 0xfb, 0x00, 0x00]),
        raise: false,
    };
    let mut cpu = Z80::new();
    cpu.reset();
    cpu.sp = 0xf000;
    cpu.step(&mut bus); // IM 1
    assert_eq!(cpu.im, 1);
    bus.raise = true;
    cpu.step(&mut bus); // EI, which cannot be interrupted after
    assert!(cpu.iff1);
    assert_eq!(cpu.pc, 3, "the instruction after EI runs first");
    cpu.step(&mut bus); // the protected instruction
    assert_eq!(cpu.pc, 4);
    cpu.step(&mut bus); // now the interrupt lands
    assert_eq!(cpu.pc, 0x0038);
    assert!(!cpu.iff1, "interrupts are disabled inside the handler");
    assert_eq!(cpu.sp, 0xeffe, "the return address was stacked");
}
