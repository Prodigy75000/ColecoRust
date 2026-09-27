// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The BIOS's graphics transforms: REFLECT_VERTICAL, REFLECT_HORIZONTAL,
//! ROTATE_90 and ENLARGE.
//!
//! Each takes BC items of VRAM table A starting at item DE, transforms them
//! one at a time through the work buffer (the cartridge header's pointer at
//! `$8006`), and writes the results from item HL on. Games use them to make
//! the facing-left copy of a sprite from the facing-right one instead of
//! storing both. In graphics mode 2 a pattern-table item also has its colours
//! carried across.
//!
//! | routine | what | items written per item read |
//! |---|---|---|
//! | REFLECT_VERTICAL | mirror left to right: each row's bits reversed | 1 |
//! | REFLECT_HORIZONTAL | mirror top to bottom: the rows reversed | 1 |
//! | ROTATE_90 | a quarter turn, bit by bit | 1 |
//! | ENLARGE | every pixel doubled: an 8x8 item becomes four | 4 |
//!
//! The real routines' side effects are kept, since the work buffer is RAM a
//! game can see: ROTATE_90 shifts its source rows through the carry, which
//! leaves them scrambled and depends on what the output bytes held before;
//! REFLECT_HORIZONTAL builds the flipped colours and then writes the
//! unflipped ones.

use super::routines::{get_vram, mode2, put_vram, ram16, Flow};
use crate::machine::ColecoBus;
use crate::z80::{Bus, Z80};

/// The cartridge header's pointer to the work buffer.
const WORK_BUFFER: u16 = 0x8006;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Vertical,
    Horizontal,
    Rotate,
    Enlarge,
}

impl Kind {
    /// The routine for an address, if it is one of these.
    pub fn at(target: u16) -> Option<Kind> {
        Some(match target {
            0x1d5a => Kind::Vertical,
            0x1d60 => Kind::Horizontal,
            0x1d66 => Kind::Rotate,
            0x1d6c => Kind::Enlarge,
            _ => return None,
        })
    }

    /// The real routine's time per item beyond its VRAM calls, T-states:
    /// the step's setup (LD HL,(nn), LD BC,8, PUSH HL, POP DE, ADD HL,BC,
    /// EX DE,HL), the CALL of its helper and the helper itself ($1F00 bit by
    /// bit, $1F4E byte by byte, $1F12 through the carry, $1EAB doubling),
    /// and the step's end (EXX, INC HL, and JR or JP back). Bit by bit on a
    /// Z80 is slow, and a game pacing itself on these (BC's Quest II draws
    /// its screens with them) runs ahead of the display if they come back
    /// early.
    fn cost(self) -> i32 {
        match self {
            Kind::Vertical => 62 + 17 + 1903 + 4 + 6 + 12,
            Kind::Horizontal => 62 + 17 + 438 + 4 + 6 + 12,
            Kind::Rotate => 62 + 17 + 2642 + 4 + 6 + 10,
            Kind::Enlarge => 62 + 17 + 4409 + 4 + 24 + 10,
        }
    }

    /// Where the real routine keeps this one's step; IX is left pointing at
    /// it.
    fn step(self) -> u16 {
        match self {
            Kind::Vertical => 0x1d96,
            Kind::Horizontal => 0x1db7,
            Kind::Rotate => 0x1de5,
            Kind::Enlarge => 0x1e07,
        }
    }
}

fn peek(bus: &mut ColecoBus, addr: u16) -> u8 {
    bus.peek(addr)
}

fn vram(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    routine: fn(&mut Z80, &mut ColecoBus) -> i32,
    table: u8,
    index: u16,
    at: u16,
    count: u16,
) -> i32 {
    cpu.set_a(table);
    cpu.set_de(index);
    cpu.set_hl(at);
    cpu.iy = count;
    routine(cpu, bus)
}

/// A nibble with every bit doubled: `abcd` to `aabbccdd`.
fn doubled(n: u8) -> u8 {
    (0..4).rev().fold(0, |r, i| {
        let b = (n >> i) & 1;
        (r << 2) | (b << 1) | b
    })
}

/// One of the four transforms (`$1D5A`, `$1D60`, `$1D66`, `$1D6C`).
///
/// The real routine keeps the caller's count and item numbers in the
/// alternate registers and advances them there, and leaves them there: BC
/// zero, DE and HL past the last item read and written, with A' the table
/// and F' the caller's flags. The main set is its working set, as its last
/// VRAM call left it. Games chain calls on the alternates, so both sets are
/// left that way, through the image's exchange stub.
pub fn transform(cpu: &mut Z80, bus: &mut ColecoBus, kind: Kind) -> Flow {
    let (table, entry_f) = (cpu.a(), cpu.f);
    let (mut src, mut dst, mut left) = (cpu.de(), cpu.hl(), cpu.bc());
    let buf = ram16(bus, WORK_BUFFER);
    let out = buf.wrapping_add(8);
    let at = |i: u16| buf.wrapping_add(i);
    let colours = table == 3 && mode2(bus);
    // LD IX,nn, JR (not for ENLARGE, which falls through), EXX, EX AF,AF',
    // PUSH IX.
    let mut c = 14 + if kind == Kind::Enlarge { 0 } else { 12 } + 4 + 4 + 15;
    // $1E5D, the colour test, with its CALL: the table first, then the mode.
    let test = 17 + 4 + 11 + 4 + 10 + 7 + if table != 3 { 12 + 7 } else if colours { 7 + 10 + 12 + 7 + 7 } else { 7 + 10 + 12 + 12 + 7 } + 10;
    // $1E89 and $1E9A, a colour item in or out with the calls around it.
    let colour_call = 17 + 7 + 4 + 11 + 4 + 10 + 16 + 14 + 17 + 10 + 10;
    loop {
        // Per item, the frame: EX AF,AF', PUSH AF, EX AF,AF', POP AF, EXX,
        // PUSH DE, EXX, POP DE, LD IY,1, LD HL,(nn), CALL $1BA3 and its RET,
        // POP IX, PUSH IX, JP (IX).
        c += 4 + 11 + 4 + 10 + 4 + 11 + 4 + 10 + 14 + 16 + 17 + 10 + 14 + 15 + 8;
        c += vram(cpu, bus, get_vram, table, src, buf, 1);
        let items = match kind {
            Kind::Vertical => {
                for i in 0..8 {
                    let v = peek(bus, at(i)).reverse_bits();
                    bus.write(at(8 + i), v);
                }
                1
            }
            Kind::Horizontal => {
                for i in 0..8 {
                    let v = peek(bus, at(7 - i));
                    bus.write(at(8 + i), v);
                }
                1
            }
            Kind::Rotate => {
                // Each output byte takes one bit from every row, shifted out
                // of the top of the row through the carry and in at the top
                // of the output byte, which pushes the output's old low bit
                // into the next row. The carry starts as ADD HL,BC left it.
                let mut carry = u8::from(buf as u32 + 8 > 0xffff);
                for j in 0..8 {
                    for k in 0..8 {
                        let r = peek(bus, at(k));
                        bus.write(at(k), (r << 1) | carry);
                        carry = r >> 7;
                        let o = peek(bus, at(8 + j));
                        bus.write(at(8 + j), (o >> 1) | (carry << 7));
                        carry = o & 1;
                    }
                }
                1
            }
            Kind::Enlarge => {
                for r in 0..8 {
                    let v = peek(bus, at(r));
                    let (hi, lo) = (doubled(v >> 4), doubled(v & 0x0f));
                    for row in [2 * r, 2 * r + 1] {
                        bus.write(at(8 + row), hi);
                        bus.write(at(24 + row), lo);
                    }
                }
                4
            }
        };
        // $1E72 (or ENLARGE's own copy of it): A from A', DE the item out,
        // HL the buffer, LD IY, and CALL $1C27 with its RET.
        c += if kind == Kind::Enlarge {
            4 + 11 + 4 + 10 + 4 + 11 + 4 + 10 + 16 + 10 + 11 + 14 + 17 + 10
        } else {
            17 + 4 + 11 + 4 + 10 + 4 + 11 + 4 + 10 + 16 + 10 + 11 + 14 + 17 + 10 + 10
        };
        c += vram(cpu, bus, put_vram, table, dst, out, items);
        // The mode 2 test reads the mode through HL; then CP 1, JR NZ.
        if table == 3 {
            cpu.set_hl(0x73c3);
        }
        c += test + 7 + if colours { 7 } else { 12 };
        if colours {
            c += colour_call + vram(cpu, bus, get_vram, 4, src, buf, 1);
            match kind {
                Kind::Vertical | Kind::Rotate => {
                    c += colour_call + vram(cpu, bus, put_vram, 4, dst, buf, 1);
                }
                Kind::Horizontal => {
                    for i in 0..8 {
                        let v = peek(bus, at(7 - i));
                        bus.write(at(8 + i), v);
                    }
                    // The step's setup again and $1F4E.
                    c += 62 + 17 + 438;
                    c += colour_call + vram(cpu, bus, put_vram, 4, dst, buf, 1);
                }
                Kind::Enlarge => {
                    for i in 0..16u16 {
                        let v = peek(bus, at(i % 8));
                        bus.write(at(8 + 2 * i), v);
                        bus.write(at(9 + 2 * i), v);
                    }
                    // The setup, $1EEA doubling the colours, then LD A,4 and
                    // the same way out as the patterns.
                    c += 62 + 17 + 1429 + 7 + 4 + 11 + 4 + 10 + 16 + 10 + 11 + 14 + 17 + 10;
                    c += vram(cpu, bus, put_vram, 4, dst, out, 4);
                }
            }
        }
        src = src.wrapping_add(1);
        dst = dst.wrapping_add(items);
        left = left.wrapping_sub(1);
        // The item's own step, then INC DE, DEC BC, LD A,B, OR C, EXX, JR NZ.
        c += kind.cost() + 6 + 6 + 4 + 4 + 4 + if left == 0 { 7 } else { 12 };
        if left == 0 {
            break;
        }
    }
    // POP IX and RET; the exchange stub's own 58 T-states come off, as the
    // real routine has no such stub.
    c += 14 + 10 - 58;
    // It ends testing the count with LD A,B; OR C, with IX on its step.
    cpu.set_a(0);
    cpu.f = 0x44;
    cpu.ix = kind.step();
    // The main set to come back, pushed under the return address for the
    // stub to pop; the alternates' values loaded where it will swap them in.
    let (at, _) = super::EXCHANGE;
    for v in [cpu.hl(), cpu.de(), cpu.bc(), u16::from_be_bytes([cpu.a(), cpu.f])] {
        cpu.sp = cpu.sp.wrapping_sub(2);
        bus.write(cpu.sp, v as u8);
        bus.write(cpu.sp.wrapping_add(1), (v >> 8) as u8);
    }
    cpu.set_a(table);
    cpu.f = entry_f;
    cpu.set_bc(0);
    cpu.set_de(src);
    cpu.set_hl(dst);
    cpu.pc = at;
    Flow::Jump(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{Coleco, Firmware};

    /// Sprite patterns at `$3800`, the work buffer at `$7100`, and two
    /// patterns written: an arrow pointing up-left, and a single pixel.
    fn machine() -> Coleco {
        let mut cart = vec![0u8; 0x2000];
        cart[6..8].copy_from_slice(&0x7100u16.to_le_bytes());
        let mut m = Coleco::new(Firmware::Hle, &cart).unwrap();
        m.bus.ram[0x3f4] = 0x00;
        m.bus.ram[0x3f5] = 0x38;
        let arrow = [0xf0, 0xc0, 0xa0, 0x90, 0x08, 0x04, 0x02, 0x01];
        m.bus.vdp.vram[0x3800..0x3808].copy_from_slice(&arrow);
        m.bus.vdp.vram[0x3808..0x3810].copy_from_slice(&[0x80, 0, 0, 0, 0, 0, 0, 0]);
        m
    }

    /// Call the routine as a game does and run until it has returned.
    fn run(m: &mut Coleco, kind: Kind, src: u16, dst: u16, count: u16) {
        m.cpu.set_a(1);
        m.cpu.set_de(src);
        m.cpu.set_hl(dst);
        m.cpu.set_bc(count);
        m.cpu.sp = 0x73ae;
        m.bus.ram[0x3ae] = 0x00;
        m.bus.ram[0x3af] = 0x90;
        transform(&mut m.cpu, &mut m.bus, kind);
        while m.cpu.pc != 0x9000 {
            m.step();
        }
    }

    fn pattern(m: &Coleco, n: usize) -> [u8; 8] {
        m.bus.vdp.vram[0x3800 + 8 * n..0x3808 + 8 * n].try_into().unwrap()
    }

    #[test]
    fn reflections_mirror_across_and_upside_down() {
        let mut m = machine();
        run(&mut m, Kind::Vertical, 0, 4, 2);
        assert_eq!(pattern(&m, 4), [0x0f, 0x03, 0x05, 0x09, 0x10, 0x20, 0x40, 0x80]);
        assert_eq!(pattern(&m, 5), [0x01, 0, 0, 0, 0, 0, 0, 0]);
        run(&mut m, Kind::Horizontal, 0, 6, 1);
        assert_eq!(pattern(&m, 6), [0x01, 0x02, 0x04, 0x08, 0x90, 0xa0, 0xc0, 0xf0]);
        assert_eq!(m.cpu.ix, 0x1db7);
    }

    /// What BC's Quest for Tires II does: EXX and EX AF,AF' after the call
    /// find the table, the count spent and the item numbers moved on, and
    /// the main set is the routine's.
    #[test]
    fn the_callers_registers_come_back_advanced_in_the_alternate_set() {
        let mut m = machine();
        run(&mut m, Kind::Enlarge, 0, 4, 2);
        assert_eq!(m.cpu.a(), 0, "main: the count test");
        assert_eq!(m.cpu.ix, 0x1e07);
        // Swap as the game does, with the stub's first two instructions.
        m.cpu.pc = 0x1f58;
        m.step();
        m.step();
        assert_eq!(m.cpu.a(), 1, "the table");
        assert_eq!((m.cpu.bc(), m.cpu.de(), m.cpu.hl()), (0, 2, 12), "count spent, 2 read, 8 written");
    }

    /// The top-left pixel goes to the top right under a quarter turn: row 0,
    /// bit 7 becomes row 0's lowest bit, as the real routine turns.
    #[test]
    fn a_quarter_turn_moves_the_corner_pixel() {
        let mut m = machine();
        run(&mut m, Kind::Rotate, 1, 4, 1);
        let p = pattern(&m, 4);
        assert_eq!(p.iter().map(|b| b.count_ones()).sum::<u32>(), 1, "{p:02X?}");
        assert_eq!(p[0], 0x01, "{p:02X?}");
    }

    /// ENLARGE writes four patterns per one: the pixel at the top left
    /// becomes a 2x2 block in the first.
    #[test]
    fn enlarge_doubles_every_pixel_into_four_patterns() {
        let mut m = machine();
        run(&mut m, Kind::Enlarge, 1, 4, 1);
        assert_eq!(pattern(&m, 4), [0xc0, 0xc0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(pattern(&m, 5), [0; 8]);
        assert_eq!(pattern(&m, 6), [0; 8]);
        assert_eq!(pattern(&m, 7), [0; 8]);
        assert_eq!(doubled(0b1010), 0b1100_1100);
    }
}
