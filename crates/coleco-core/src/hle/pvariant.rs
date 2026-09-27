// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The "P" entries: the same routines with their parameters written after
//! the CALL instead of passed in registers.
//!
//! A game calls, say, WRITE_VRAMP and follows the CALL with three words.
//! Each word points at a parameter (or is 0, and the next word is the
//! address of a variable that holds the pointer). The routine's descriptor
//! says, per parameter, whether it wants the pointer itself or that many
//! bytes copied from where it points. The BIOS's parser (`$0098`) gathers
//! them into RAM from `$73BA` (some entries use a later address), moves the
//! return address past the words, and the entry loads its registers from
//! that RAM and falls into the ordinary routine. Zaxxon and Donkey Kong for
//! Adam call them; nothing else in the corpus does.
//!
//! The parser leaves A zero, F from its last compare (`$42`), BC past the
//! descriptor, DE past what it wrote and HL on the entry's next
//! instruction, and the ordinary routine starts from those, so they are set
//! as it leaves them.

use super::routines::{call, ram16, set_ram16, Flow};
use crate::machine::ColecoBus;
use crate::z80::{Bus, Z80};

/// A parameter passed by its pointer (the descriptor's `$FFFE`) rather than
/// by copying bytes from it.
const PTR: u16 = 0xfffe;

/// One entry: its address, its descriptor's address in the real BIOS (for
/// BC), where the parameters go, their sizes, and the routine it becomes.
struct Entry {
    at: u16,
    desc: u16,
    dest: u16,
    params: &'static [u16],
    routine: u16,
}

const ENTRIES: [Entry; 17] = [
    Entry { at: 0x0203, desc: 0x01fd, dest: 0x73ba, params: &[1, 2], routine: 0x0213 },
    Entry { at: 0x0251, desc: 0x024d, dest: 0x73ba, params: &[1], routine: 0x025e },
    Entry { at: 0x0488, desc: 0x0482, dest: 0x73ba, params: &[PTR, 1], routine: 0x04a3 },
    Entry { at: 0x06c7, desc: 0x06c1, dest: 0x73ba, params: &[2, 1], routine: 0x06d8 },
    Entry { at: 0x0655, desc: 0x064f, dest: 0x73ba, params: &[1, PTR], routine: 0x0664 },
    Entry { at: 0x0f9a, desc: 0x0f94, dest: 0x73ba, params: &[2, 2], routine: 0x0faa },
    Entry { at: 0x0fb8, desc: 0x0fb4, dest: 0x73be, params: &[1], routine: 0x0fc4 },
    Entry { at: 0x1044, desc: 0x103e, dest: 0x73bf, params: &[1, 2], routine: 0x1053 },
    Entry { at: 0x10bf, desc: 0x10bb, dest: 0x73c2, params: &[1], routine: 0x10cb },
    Entry { at: 0x1b0e, desc: 0x1b08, dest: 0x73ba, params: &[1, 2], routine: 0x1b1d },
    Entry { at: 0x1b8c, desc: 0x1b80, dest: 0x73ba, params: &[1, 1, 1, PTR, 2], routine: 0x1ba3 },
    Entry { at: 0x1c10, desc: 0x1c04, dest: 0x73ba, params: &[1, 1, 1, PTR, 2], routine: 0x1c27 },
    Entry { at: 0x1c5a, desc: 0x1c56, dest: 0x73ba, params: &[1], routine: 0x1c66 },
    Entry { at: 0x1c76, desc: 0x1c72, dest: 0x73ba, params: &[1], routine: 0x1c82 },
    Entry { at: 0x1cbc, desc: 0x1cb6, dest: 0x73ba, params: &[1, 1], routine: 0x1cca },
    Entry { at: 0x1ced, desc: 0x1ce5, dest: 0x73ba, params: &[PTR, 2, 2], routine: 0x1d01 },
    Entry { at: 0x1d2a, desc: 0x1d22, dest: 0x73ba, params: &[PTR, 2, 2], routine: 0x1d3e },
];

/// Whether `pc` is a P entry.
pub fn is_entry(pc: u16) -> bool {
    ENTRIES.iter().any(|e| e.at == pc)
}

/// The parser (`$0098`): the parameters after the caller's CALL into RAM
/// at `dest`, and the return address moved past them. Returns the cycles.
fn gather(cpu: &mut Z80, bus: &mut ColecoBus, e: &Entry) -> i32 {
    let mut words = ram16(bus, cpu.sp);
    let mut dest = e.dest;
    // The entry's LD BC,nn, LD DE,nn and CALL; the parser's POP HL, EX
    // (SP),HL, PUSH HL, the count's load, EX (SP),HL, PUSH DE.
    let mut c = 10 + 10 + 17 + 10 + 19 + 11 + 7 + 4 + 6 + 7 + 6 + 4 + 19 + 11;
    let count = e.params.len();
    for (k, &size) in e.params.iter().enumerate() {
        let mut ptr = ram16(bus, words);
        words = words.wrapping_add(2);
        // The word, PUSH HL, LD A,E, OR D, JP NZ; then INC BC, LD A,(BC),
        // RLCA, JP NC.
        c += 7 + 6 + 7 + 6 + 11 + 4 + 4 + 10 + 6 + 7 + 4 + 10;
        if ptr == 0 {
            // POP HL, the variable's address, PUSH HL, EX DE,HL, its word.
            c += 10 + 7 + 6 + 7 + 6 + 11 + 4 + 7 + 6 + 7;
            let var = ram16(bus, words);
            words = words.wrapping_add(2);
            ptr = ram16(bus, var);
        }
        if size & 0x8000 != 0 {
            // INC BC, POP HL, EX (SP),HL, the pointer stored, INC HL x2.
            c += 6 + 10 + 19 + 7 + 6 + 7 + 6;
            set_ram16(bus, dest, ptr);
            dest = dest.wrapping_add(2);
        } else {
            // POP HL, EX (SP),HL, PUSH HL, RRCA, LD H,A, DEC BC, LD A,(BC),
            // LD L,A, EX (SP),HL, INC BC x2; per byte LD A,(DE), LD (HL),A,
            // INC HL, INC DE, EX (SP),HL, DEC HL, XOR A, CP L, JP NZ, then
            // EX (SP),HL and JP back, or for the last CP H, JP Z, POP HL, JP.
            c += 10 + 19 + 11 + 4 + 4 + 6 + 7 + 4 + 19 + 12;
            // DEC HL; test for zero: a size of 0 copies 65536 bytes.
            let n = if size == 0 { 0x10000 } else { size as u32 };
            for i in 0..n {
                let v = bus.peek(ptr.wrapping_add(i as u16));
                bus.write(dest, v);
                dest = dest.wrapping_add(1);
            }
            c += 98 * (n as i32 - 1) + 103;
        }
        // POP DE, EX (SP),HL, DEC HL, XOR A, CP H, JP NZ, CP L, JP Z; then
        // for another parameter EX (SP),HL, PUSH HL, EX DE,HL, JP.
        c += 10 + 19 + 6 + 4 + 4 + 10 + 4 + 10;
        if k + 1 < count {
            c += 19 + 11 + 4 + 10;
        }
    }
    // POP HL, EX DE,HL, EX (SP),HL, JP (HL).
    c += 10 + 4 + 19 + 4;
    set_ram16(bus, cpu.sp, words);
    cpu.set_a(0);
    cpu.f = 0x42;
    cpu.set_bc(e.desc.wrapping_add(2 + 2 * e.params.len() as u16));
    cpu.set_de(dest);
    cpu.set_hl(e.at.wrapping_add(9));
    c
}

/// Load the registers the ordinary routine takes, as each entry does.
/// Returns the loads' time.
fn load(cpu: &mut Z80, bus: &mut ColecoBus, at: u16) -> i32 {
    let t = match at {
        0x0203 => 13 + 4 + 16,
        0x0251 => 13 + 4,
        // LD HL,(nn), LD E,(HL), INC HL, LD D,(HL), EX DE,HL, LD A,(nn),
        // CP 0, and JR Z to OR A, or SCF and JR.
        0x0488 => {
            16 + 7 + 6 + 7 + 4 + 13 + 7 + if bus.peek(0x73bc) == 0 { 12 + 4 } else { 7 + 4 + 12 }
        }
        0x06c7 => 20 + 13 + 4,
        0x0655 | 0x1b0e | 0x1044 => 13 + 16,
        0x0f9a => 16 + 20,
        0x0fb8 | 0x10bf | 0x1c5a | 0x1c76 => 13,
        0x1b8c | 0x1c10 => 13 + 20 + 20 + 16,
        0x1cbc => 16 + 4 + 4,
        _ => 16 + 20 + 20,
    };
    let byte = |bus: &mut ColecoBus, a: u16| bus.peek(a);
    match at {
        // B, and HL for SOUND_INIT.
        0x0203 => {
            let b = byte(bus, 0x73ba);
            cpu.set_a(b);
            cpu.set_bc(u16::from_be_bytes([b, cpu.bc() as u8]));
            cpu.set_hl(ram16(bus, 0x73bb));
        }
        // PLAY_IT's song in B, loaded through A.
        0x0251 => {
            let b = byte(bus, 0x73ba);
            cpu.set_a(b);
            cpu.set_bc(u16::from_be_bytes([b, cpu.bc() as u8]));
        }
        // ACTIVATE: HL the word the pointer points at, carry from the flag.
        0x0488 => {
            let p = ram16(bus, 0x73ba);
            cpu.set_de(p.wrapping_add(1));
            cpu.set_hl(ram16(bus, p));
            let a = byte(bus, 0x73bc);
            cpu.set_a(a);
            cpu.f = if a == 0 {
                0x44
            } else {
                let cp = super::routines::cp_flags(a, 0);
                (cp & 0xc4) | (a & 0x28) | 0x01
            };
        }
        // PUTOBJ: IX the object, B through A.
        0x06c7 => {
            cpu.ix = ram16(bus, 0x73ba);
            let b = byte(bus, 0x73bc);
            cpu.set_a(b);
            cpu.set_bc(u16::from_be_bytes([b, cpu.bc() as u8]));
        }
        // A and HL: INIT_WRITER and INIT_TABLE.
        0x0655 | 0x1b0e => {
            cpu.set_a(byte(bus, 0x73ba));
            cpu.set_hl(ram16(bus, 0x73bb));
        }
        0x0f9a => {
            cpu.set_hl(ram16(bus, 0x73ba));
            cpu.set_de(ram16(bus, 0x73bc));
        }
        0x0fb8 => cpu.set_a(byte(bus, 0x73be)),
        0x1044 => {
            cpu.set_hl(ram16(bus, 0x73c0));
            cpu.set_a(byte(bus, 0x73bf));
        }
        0x10bf => cpu.set_a(byte(bus, 0x73c2)),
        // GET_VRAM and PUT_VRAM: A, DE, IY, HL.
        0x1b8c | 0x1c10 => {
            cpu.set_a(byte(bus, 0x73ba));
            cpu.set_de(ram16(bus, 0x73bb));
            cpu.iy = ram16(bus, 0x73bf);
            cpu.set_hl(ram16(bus, 0x73bd));
        }
        0x1c5a | 0x1c76 => cpu.set_a(byte(bus, 0x73ba)),
        // WRITE_REGISTER: the first byte is the register (B), the second
        // its value (C), through HL.
        0x1cbc => {
            let hl = ram16(bus, 0x73ba);
            cpu.set_hl(hl);
            cpu.set_bc(hl.swap_bytes());
        }
        // WRITE_VRAM and READ_VRAM: HL, DE, BC.
        _ => {
            cpu.set_hl(ram16(bus, 0x73ba));
            cpu.set_de(ram16(bus, 0x73bc));
            cpu.set_bc(ram16(bus, 0x73be));
        }
    }
    t
}

/// A P entry: gather, load, and run the ordinary routine. `None` when that
/// routine is not written.
pub fn run(pc: u16, cpu: &mut Z80, bus: &mut ColecoBus) -> Option<Flow> {
    let e = ENTRIES.iter().find(|e| e.at == pc)?;
    let c = gather(cpu, bus, e);
    let c = c + load(cpu, bus, e.at);
    Some(match call(e.routine, cpu, bus)? {
        Flow::Ret(r) => Flow::Ret(r + c),
        Flow::Jump(r) => Flow::Jump(r + c),
    })
}

#[cfg(test)]
mod tests {
    use crate::machine::{Coleco, Firmware};

    /// WRITE_VRAMP from a cartridge: `CALL $1FA9` and three words after it,
    /// a pointer to the bytes (by reference), and pointers to the VRAM
    /// address and the count (copied). The bytes land in VRAM and the call
    /// returns past the words.
    #[test]
    fn write_vram_p_takes_its_parameters_after_the_call() {
        let mut cart = vec![0u8; 0x2000];
        cart[0..2].copy_from_slice(&[0x55, 0xaa]);
        cart[0x0a..0x0c].copy_from_slice(&[0x00, 0x81]);
        // $8100: CALL $1FA9; DW $8200, $8210, 0, $7010; JR $
        cart[0x100..0x103].copy_from_slice(&[0xcd, 0xa9, 0x1f]);
        cart[0x103..0x10b].copy_from_slice(&[0x00, 0x82, 0x10, 0x82, 0x00, 0x00, 0x10, 0x70]);
        cart[0x10b..0x10d].copy_from_slice(&[0x18, 0xfe]);
        cart[0x200..0x204].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        cart[0x210..0x212].copy_from_slice(&0x1234u16.to_le_bytes());
        // The count sits behind a pointer held in RAM at $7010: 0, then its
        // address, the parser's second form.
        cart[0x220..0x222].copy_from_slice(&3u16.to_le_bytes());
        let mut m = Coleco::new(Firmware::Hle, &cart).unwrap();
        m.bus.ram[0x010] = 0x20;
        m.bus.ram[0x011] = 0x82;
        m.run_frame();
        assert_eq!(m.cpu.pc, 0x810b, "back past the four words, spinning");
        assert_eq!(&m.bus.vdp.vram[0x1234..0x1238], &[0xde, 0xad, 0xbe, 0x00]);
        assert_eq!(m.hle_log.unimplemented.len(), 0);
    }
}
