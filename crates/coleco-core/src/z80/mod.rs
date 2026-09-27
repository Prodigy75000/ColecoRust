// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! Clean-room Zilog Z80.
//!
//! **Lifted from MegaRust** (`crates/md-core/src/cpu/z80/`, at `origin/main`
//! 3e97ca7, 2026-09-27), where it passes ZEXALL 79/79 and already runs the
//! SG-1000, which is this machine's near-twin. `mod.rs`, `exec.rs` and
//! `tests.rs` are kept byte-identical to MegaRust below this paragraph on
//! purpose, so a fix found on either side carries over as a plain diff. Change
//! the CPU here only for a real Z80 defect, and tell MegaRust when you do.
//! Paths below, such as `docs/cpu/`, are MegaRust's.
//!
//! Encodings and timings from the Zilog Z80 CPU User's Manual (UM0080), in
//! `docs/cpu/`. The undocumented behaviour the manual does not cover, and which
//! ZEXALL checks, is implemented from the exerciser's own expectations:
//!
//!   * **Bits 5 and 3 of F** (often called Y and X) are not flags. They are
//!     copies of bits 5 and 3 of the last result, and for `BIT n,(HL)` they come
//!     from the internal address latch instead.
//!   * **MEMPTR / WZ**, an internal register the programmer cannot see, is
//!     observable through exactly one instruction: `BIT n,(HL)`, which sources
//!     its X and Y flags from WZ's high byte. Every instruction that touches
//!     memory updates it, and the rules differ per instruction.
//!   * **SCF and CCF** derive X and Y from `A` **or** from the previous flags,
//!     depending on whether the instruction before them set flags. That is what
//!     the `q` latch below tracks.
//!   * **The DD/FD prefixes** rewrite H and L to IXH/IXL, *except* when the
//!     instruction reaches memory through `(IX+d)`, where the other operand
//!     stays a plain register.
//!
//! The Mega Drive uses this as its sound CPU; on the 8-bit rungs of the family
//! it is the main CPU. As with the 68000 it takes an injected [`Bus`] and never
//! touches system memory, so one file serves both roles.

mod exec;
#[cfg(test)]
mod tests;

use crate::save::{LoadError, ReadCursor, SaveState, WriteCursor};

/// Flag bits. `Y` and `X` are the undocumented copies of result bits 5 and 3.
pub mod flag {
    pub const C: u8 = 0x01;
    pub const N: u8 = 0x02;
    /// Parity for logical ops, overflow for arithmetic.
    pub const PV: u8 = 0x04;
    pub const X: u8 = 0x08;
    pub const H: u8 = 0x10;
    pub const Y: u8 = 0x20;
    pub const Z: u8 = 0x40;
    pub const S: u8 = 0x80;
    /// The two undocumented bits together.
    pub const XY: u8 = X | Y;
}

/// Everything the Z80 can reach. Memory and I/O are separate spaces on this
/// chip, which is why they are separate methods rather than an address range.
pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, val: u8);
    fn input(&mut self, port: u16) -> u8;
    fn output(&mut self, port: u16, val: u8);
    /// True while an interrupt is being requested on the maskable input.
    fn irq(&mut self) -> bool {
        false
    }
    /// Byte the device places on the bus during an interrupt acknowledge. Only
    /// interrupt mode 0 and mode 2 read it; the Mega Drive drives none, so the
    /// default is the `$FF` an undriven bus floats to, which in mode 0 is `RST 38h`.
    fn irq_vector(&mut self) -> u8 {
        0xff
    }
}

/// Register indices in the classic `r` field order.
const B: usize = 0;
const C: usize = 1;
const D: usize = 2;
const E: usize = 3;
const H: usize = 4;
const L: usize = 5;
/// `110` in the `r` field means "through (HL)", not a register.
const MEM: usize = 6;
const A: usize = 7;

/// Which register file `H`/`L` and `(HL)` refer to for the current instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Index {
    Hl,
    Ix,
    Iy,
}

pub struct Z80 {
    /// B C D E H L, a hole where `(HL)` sits, then A.
    r: [u8; 8],
    pub f: u8,
    /// The shadow set, swapped by `EX AF,AF'` and `EXX`.
    r_alt: [u8; 8],
    f_alt: u8,

    pub ix: u16,
    pub iy: u16,
    pub sp: u16,
    pub pc: u16,

    /// Interrupt vector base and the refresh counter. `r` increments on every
    /// opcode fetch and its top bit is never touched by that increment, which
    /// software does notice.
    pub i: u8,
    pub rr: u8,

    pub iff1: bool,
    pub iff2: bool,
    pub im: u8,
    pub halted: bool,
    /// Interrupts accepted. A bring-up diagnostic, not machine state: "is the
    /// sound driver being ticked at all" has no other honest answer.
    pub irqs_taken: u32,
    /// True for exactly one instruction after `EI`, during which an interrupt
    /// cannot be taken. Without it, `EI; RET` from a handler re-enters at once.
    ei_pending: bool,

    /// The internal address latch, WZ. Invisible except through `BIT n,(HL)`.
    memptr: u16,
    /// Flags as the last instruction left them, or 0 if it set none. `SCF` and
    /// `CCF` read their undocumented bits from this.
    q: u8,

    cycles: i32,
}

/// Parity of a byte, as the PV flag reports it for logical operations: set when
/// the number of set bits is even.
fn parity(v: u8) -> bool {
    v.count_ones() % 2 == 0
}

fn sz53(v: u8) -> u8 {
    (v & (flag::S | flag::XY)) | if v == 0 { flag::Z } else { 0 }
}

fn sz53p(v: u8) -> u8 {
    sz53(v) | if parity(v) { flag::PV } else { 0 }
}

impl Z80 {
    pub fn new() -> Self {
        // Power-on leaves everything set, which software can and does observe.
        Z80 {
            r: [0xff; 8],
            f: 0xff,
            r_alt: [0xff; 8],
            f_alt: 0xff,
            ix: 0xffff,
            iy: 0xffff,
            sp: 0xffff,
            pc: 0,
            i: 0,
            rr: 0,
            iff1: false,
            iff2: false,
            im: 0,
            halted: false,
            irqs_taken: 0,
            ei_pending: false,
            memptr: 0,
            q: 0,
            cycles: 0,
        }
    }

    /// Reset: PC and the interrupt state clear, everything else is left alone,
    /// which is what the chip does.
    pub fn reset(&mut self) {
        self.pc = 0;
        self.i = 0;
        self.rr = 0;
        self.iff1 = false;
        self.iff2 = false;
        self.im = 0;
        self.halted = false;
        self.ei_pending = false;
        self.memptr = 0;
        self.q = 0;
    }

    pub fn a(&self) -> u8 {
        self.r[A]
    }
    pub fn set_a(&mut self, v: u8) {
        self.r[A] = v;
    }
    pub fn bc(&self) -> u16 {
        u16::from_be_bytes([self.r[B], self.r[C]])
    }
    pub fn de(&self) -> u16 {
        u16::from_be_bytes([self.r[D], self.r[E]])
    }
    pub fn hl(&self) -> u16 {
        u16::from_be_bytes([self.r[H], self.r[L]])
    }
    pub fn set_bc(&mut self, v: u16) {
        let b = v.to_be_bytes();
        self.r[B] = b[0];
        self.r[C] = b[1];
    }
    pub fn set_de(&mut self, v: u16) {
        let b = v.to_be_bytes();
        self.r[D] = b[0];
        self.r[E] = b[1];
    }
    #[cfg(test)]
    pub(crate) fn set_bc_for_test(&mut self, v: u16) {
        self.set_bc(v);
    }
    #[cfg(test)]
    pub(crate) fn set_de_for_test(&mut self, v: u16) {
        self.set_de(v);
    }
    #[cfg(test)]
    pub(crate) fn set_hl_for_test(&mut self, v: u16) {
        self.set_hl(v);
    }

    pub fn set_hl(&mut self, v: u16) {
        let b = v.to_be_bytes();
        self.r[H] = b[0];
        self.r[L] = b[1];
    }

    /// The 16-bit register the current index selects, for the instructions that
    /// operate on HL as a pair.
    fn idx16(&self, idx: Index) -> u16 {
        match idx {
            Index::Hl => self.hl(),
            Index::Ix => self.ix,
            Index::Iy => self.iy,
        }
    }

    fn set_idx16(&mut self, idx: Index, v: u16) {
        match idx {
            Index::Hl => self.set_hl(v),
            Index::Ix => self.ix = v,
            Index::Iy => self.iy = v,
        }
    }

    // ---- fetch and bus helpers ----

    fn tick(&mut self, n: i32) {
        self.cycles += n;
    }

    fn fetch<Bs: Bus>(&mut self, bus: &mut Bs) -> u8 {
        let v = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        // The refresh counter counts in its low seven bits only.
        self.rr = (self.rr & 0x80) | (self.rr.wrapping_add(1) & 0x7f);
        self.tick(4);
        v
    }

    fn fetch16<Bs: Bus>(&mut self, bus: &mut Bs) -> u16 {
        let lo = self.read_operand(bus);
        let hi = self.read_operand(bus);
        u16::from_le_bytes([lo, hi])
    }

    /// An operand byte from the instruction stream. Unlike an opcode fetch this
    /// does not bump the refresh counter.
    fn read_operand<Bs: Bus>(&mut self, bus: &mut Bs) -> u8 {
        let v = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        self.tick(3);
        v
    }

    fn read_mem<Bs: Bus>(&mut self, bus: &mut Bs, addr: u16) -> u8 {
        self.tick(3);
        bus.read(addr)
    }

    fn write_mem<Bs: Bus>(&mut self, bus: &mut Bs, addr: u16, val: u8) {
        self.tick(3);
        bus.write(addr, val);
    }

    fn push16<Bs: Bus>(&mut self, bus: &mut Bs, v: u16) {
        let b = v.to_be_bytes();
        self.sp = self.sp.wrapping_sub(1);
        self.write_mem(bus, self.sp, b[0]);
        self.sp = self.sp.wrapping_sub(1);
        self.write_mem(bus, self.sp, b[1]);
    }

    fn pop16<Bs: Bus>(&mut self, bus: &mut Bs) -> u16 {
        let lo = self.read_mem(bus, self.sp);
        self.sp = self.sp.wrapping_add(1);
        let hi = self.read_mem(bus, self.sp);
        self.sp = self.sp.wrapping_add(1);
        u16::from_le_bytes([lo, hi])
    }

    /// The address `(HL)` resolves to, taking the displacement byte when an
    /// index prefix is in force.
    fn mem_addr<Bs: Bus>(&mut self, bus: &mut Bs, idx: Index) -> u16 {
        match idx {
            Index::Hl => self.hl(),
            _ => {
                let d = self.read_operand(bus) as i8 as i16;
                let a = self.idx16(idx).wrapping_add(d as u16);
                // An indexed access always latches its address.
                self.memptr = a;
                self.tick(5);
                a
            }
        }
    }

    /// Read register `n`, resolving `(HL)` and the index rewrite. `mem` is the
    /// address computed once by the caller when the instruction touches memory.
    fn get_r<Bs: Bus>(&mut self, bus: &mut Bs, n: usize, idx: Index, mem: u16) -> u8 {
        match n {
            MEM => self.read_mem(bus, mem),
            H | L if idx != Index::Hl => {
                let v = self.idx16(idx);
                if n == H {
                    (v >> 8) as u8
                } else {
                    v as u8
                }
            }
            _ => self.r[n],
        }
    }

    fn set_r<Bs: Bus>(&mut self, bus: &mut Bs, n: usize, idx: Index, mem: u16, val: u8) {
        match n {
            MEM => self.write_mem(bus, mem, val),
            H | L if idx != Index::Hl => {
                let v = self.idx16(idx);
                let nv = if n == H {
                    (v & 0x00ff) | ((val as u16) << 8)
                } else {
                    (v & 0xff00) | val as u16
                };
                self.set_idx16(idx, nv);
            }
            _ => self.r[n] = val,
        }
    }

    // ---- ALU ----

    fn add8(&mut self, v: u8, carry: bool) {
        let a = self.r[A];
        let c = u16::from(carry);
        let res16 = a as u16 + v as u16 + c;
        let res = res16 as u8;
        let half = (a & 0x0f) + (v & 0x0f) + c as u8 > 0x0f;
        // Overflow: both operands agree in sign and the result disagrees.
        let ovf = (a ^ res) & (v ^ res) & 0x80 != 0;
        self.set_flags(
            sz53(res)
                | if half { flag::H } else { 0 }
                | if ovf { flag::PV } else { 0 }
                | if res16 > 0xff { flag::C } else { 0 },
        );
        self.r[A] = res;
    }

    fn sub8(&mut self, v: u8, carry: bool, store: bool) {
        let a = self.r[A];
        let c = u16::from(carry);
        let res16 = (a as u16).wrapping_sub(v as u16).wrapping_sub(c);
        let res = res16 as u8;
        let half = ((a & 0x0f) as i16 - (v & 0x0f) as i16 - c as i16) < 0;
        let ovf = (a ^ v) & (a ^ res) & 0x80 != 0;
        self.set_flags(
            sz53(res)
                | flag::N
                | if half { flag::H } else { 0 }
                | if ovf { flag::PV } else { 0 }
                | if res16 > 0xff { flag::C } else { 0 },
        );
        if store {
            self.r[A] = res;
        } else {
            // CP takes its undocumented bits from the *operand*, not the result,
            // which is the one place they differ from every other ALU op.
            self.set_flags((self.f & !flag::XY) | (v & flag::XY));
        }
    }

    fn and8(&mut self, v: u8) {
        self.r[A] &= v;
        self.set_flags(sz53p(self.r[A]) | flag::H);
    }

    fn or8(&mut self, v: u8) {
        self.r[A] |= v;
        self.set_flags(sz53p(self.r[A]));
    }

    fn xor8(&mut self, v: u8) {
        self.r[A] ^= v;
        self.set_flags(sz53p(self.r[A]));
    }

    fn inc8(&mut self, v: u8) -> u8 {
        let res = v.wrapping_add(1);
        self.set_flags(
            (self.f & flag::C)
                | sz53(res)
                | if v & 0x0f == 0x0f { flag::H } else { 0 }
                | if res == 0x80 { flag::PV } else { 0 },
        );
        res
    }

    fn dec8(&mut self, v: u8) -> u8 {
        let res = v.wrapping_sub(1);
        self.set_flags(
            (self.f & flag::C)
                | flag::N
                | sz53(res)
                | if v & 0x0f == 0 { flag::H } else { 0 }
                | if res == 0x7f { flag::PV } else { 0 },
        );
        res
    }

    fn add16(&mut self, a: u16, b: u16) -> u16 {
        let res = a.wrapping_add(b);
        self.memptr = a.wrapping_add(1);
        let half = (a & 0x0fff) + (b & 0x0fff) > 0x0fff;
        self.set_flags(
            (self.f & (flag::S | flag::Z | flag::PV))
                | (((res >> 8) as u8) & flag::XY)
                | if half { flag::H } else { 0 }
                | if (a as u32 + b as u32) > 0xffff {
                    flag::C
                } else {
                    0
                },
        );
        self.tick(7);
        res
    }

    fn adc16(&mut self, a: u16, b: u16) -> u16 {
        let c = u32::from(self.f & flag::C != 0);
        let full = a as u32 + b as u32 + c;
        let res = full as u16;
        self.memptr = a.wrapping_add(1);
        let half = (a & 0x0fff) as u32 + (b & 0x0fff) as u32 + c > 0x0fff;
        let ovf = (a ^ res) & (b ^ res) & 0x8000 != 0;
        self.set_flags(
            (((res >> 8) as u8) & (flag::S | flag::XY))
                | if res == 0 { flag::Z } else { 0 }
                | if half { flag::H } else { 0 }
                | if ovf { flag::PV } else { 0 }
                | if full > 0xffff { flag::C } else { 0 },
        );
        self.tick(7);
        res
    }

    fn sbc16(&mut self, a: u16, b: u16) -> u16 {
        let c = u32::from(self.f & flag::C != 0);
        let full = (a as u32).wrapping_sub(b as u32).wrapping_sub(c);
        let res = full as u16;
        self.memptr = a.wrapping_add(1);
        let half = ((a & 0x0fff) as i32 - (b & 0x0fff) as i32 - c as i32) < 0;
        let ovf = (a ^ b) & (a ^ res) & 0x8000 != 0;
        self.set_flags(
            (((res >> 8) as u8) & (flag::S | flag::XY))
                | flag::N
                | if res == 0 { flag::Z } else { 0 }
                | if half { flag::H } else { 0 }
                | if ovf { flag::PV } else { 0 }
                | if full > 0xffff { flag::C } else { 0 },
        );
        self.tick(7);
        res
    }

    /// The decimal adjust. Its correction table is the one part of the Z80 that
    /// is easier to state as rules than to derive.
    fn daa(&mut self) {
        let a = self.r[A];
        let mut correction = 0u8;
        let mut carry = self.f & flag::C != 0;
        if self.f & flag::H != 0 || (a & 0x0f) > 9 {
            correction |= 0x06;
        }
        if carry || a > 0x99 {
            correction |= 0x60;
            carry = true;
        }
        let res = if self.f & flag::N != 0 {
            a.wrapping_sub(correction)
        } else {
            a.wrapping_add(correction)
        };
        let half = if self.f & flag::N != 0 {
            self.f & flag::H != 0 && (a & 0x0f) < 6
        } else {
            (a & 0x0f) > 9
        };
        self.set_flags(
            sz53p(res)
                | (self.f & flag::N)
                | if half { flag::H } else { 0 }
                | if carry { flag::C } else { 0 },
        );
        self.r[A] = res;
    }

    // ---- rotates and shifts ----

    fn rlc(&mut self, v: u8) -> u8 {
        let c = v >> 7;
        let res = (v << 1) | c;
        self.set_flags(sz53p(res) | c);
        res
    }
    fn rrc(&mut self, v: u8) -> u8 {
        let c = v & 1;
        let res = (v >> 1) | (c << 7);
        self.set_flags(sz53p(res) | c);
        res
    }
    fn rl(&mut self, v: u8) -> u8 {
        let old = self.f & flag::C;
        let res = (v << 1) | old;
        self.set_flags(sz53p(res) | (v >> 7));
        res
    }
    fn rr(&mut self, v: u8) -> u8 {
        let old = self.f & flag::C;
        let res = (v >> 1) | (old << 7);
        self.set_flags(sz53p(res) | (v & 1));
        res
    }
    fn sla(&mut self, v: u8) -> u8 {
        let res = v << 1;
        self.set_flags(sz53p(res) | (v >> 7));
        res
    }
    fn sra(&mut self, v: u8) -> u8 {
        let res = (v >> 1) | (v & 0x80);
        self.set_flags(sz53p(res) | (v & 1));
        res
    }
    /// The undocumented shift: like SLA but it shifts a 1 in.
    fn sll(&mut self, v: u8) -> u8 {
        let res = (v << 1) | 1;
        self.set_flags(sz53p(res) | (v >> 7));
        res
    }
    fn srl(&mut self, v: u8) -> u8 {
        let res = v >> 1;
        self.set_flags(sz53p(res) | (v & 1));
        res
    }

    /// The A-register rotates, which differ from the CB ones: they leave S, Z
    /// and PV alone and take their undocumented bits from the result.
    fn rlca(&mut self) {
        let v = self.r[A];
        let c = v >> 7;
        self.r[A] = (v << 1) | c;
        self.set_flags((self.f & (flag::S | flag::Z | flag::PV)) | (self.r[A] & flag::XY) | c);
    }
    fn rrca(&mut self) {
        let v = self.r[A];
        let c = v & 1;
        self.r[A] = (v >> 1) | (c << 7);
        self.set_flags((self.f & (flag::S | flag::Z | flag::PV)) | (self.r[A] & flag::XY) | c);
    }
    fn rla(&mut self) {
        let v = self.r[A];
        let old = self.f & flag::C;
        self.r[A] = (v << 1) | old;
        self.set_flags(
            (self.f & (flag::S | flag::Z | flag::PV)) | (self.r[A] & flag::XY) | (v >> 7),
        );
    }
    fn rra(&mut self) {
        let v = self.r[A];
        let old = self.f & flag::C;
        self.r[A] = (v >> 1) | (old << 7);
        self.set_flags(
            (self.f & (flag::S | flag::Z | flag::PV)) | (self.r[A] & flag::XY) | (v & 1),
        );
    }

    fn cpl(&mut self) {
        self.r[A] = !self.r[A];
        self.set_flags(
            (self.f & (flag::S | flag::Z | flag::PV | flag::C))
                | flag::H
                | flag::N
                | (self.r[A] & flag::XY),
        );
    }

    /// SCF and CCF take their undocumented bits from `A` OR'd with the previous
    /// flags, but only when the instruction before them set flags. `q` is zero
    /// when it did not, which makes the difference observable.
    fn scf_ccf(&mut self, complement: bool) {
        let carry = self.f & flag::C != 0;
        let xy = (self.r[A] | (self.f & !self.q)) & flag::XY;
        self.set_flags((self.f & (flag::S | flag::Z | flag::PV)) | xy);
        if complement {
            self.f |= if carry { flag::H } else { 0 } | if carry { 0 } else { flag::C };
        } else {
            self.f |= flag::C;
        }
    }

    // ---- small accessors the decode leans on ----

    /// The refresh counter advances on every opcode fetch, in its low seven bits
    /// only: the top bit is software's to set and the counter never carries into
    /// it. Software does read `R` back, for entropy among other things.
    fn bump_refresh(&mut self) {
        self.rr = (self.rr & 0x80) | (self.rr.wrapping_add(1) & 0x7f);
    }

    /// Read a register by its `r`-field index.
    pub fn reg(&self, n: usize) -> u8 {
        self.r[n]
    }

    fn set_reg(&mut self, n: usize, v: u8) {
        self.r[n] = v;
    }

    /// The single place flags are written, so `q` always agrees with whether the
    /// instruction just gone by set any. `SCF` and `CCF` are the only readers,
    /// and getting this wrong is invisible until ZEXALL says so.
    fn set_flags(&mut self, v: u8) {
        self.f = v;
        self.q = v;
    }

    fn swap_af(&mut self) {
        std::mem::swap(&mut self.r[A], &mut self.r_alt[A]);
        std::mem::swap(&mut self.f, &mut self.f_alt);
    }

    fn swap_exx(&mut self) {
        for i in [B, C, D, E, H, L] {
            std::mem::swap(&mut self.r[i], &mut self.r_alt[i]);
        }
    }

    fn swap_de_hl(&mut self) {
        let de = self.de();
        let hl = self.hl();
        self.set_de(hl);
        self.set_hl(de);
    }

    /// The displacement half of an indexed address, without the five internal
    /// cycles [`Z80::mem_addr`] charges. `LD (IX+d),n` needs the displacement
    /// before its immediate but pays a different amount of internal time.
    fn mem_addr_no_tick<Bs: Bus>(&mut self, bus: &mut Bs, idx: Index) -> u16 {
        match idx {
            Index::Hl => self.hl(),
            _ => {
                let d = self.read_operand(bus) as i8 as i16;
                let a = self.idx16(idx).wrapping_add(d as u16);
                self.memptr = a;
                a
            }
        }
    }

    fn condition(&self, cc: u8) -> bool {
        match cc {
            0 => self.f & flag::Z == 0,
            1 => self.f & flag::Z != 0,
            2 => self.f & flag::C == 0,
            3 => self.f & flag::C != 0,
            4 => self.f & flag::PV == 0,
            5 => self.f & flag::PV != 0,
            6 => self.f & flag::S == 0,
            _ => self.f & flag::S != 0,
        }
    }
}

impl Default for Z80 {
    fn default() -> Self {
        Self::new()
    }
}

impl SaveState for Z80 {
    fn save(&self, w: &mut WriteCursor) {
        w.bytes(&self.r);
        w.u8(self.f);
        w.bytes(&self.r_alt);
        w.u8(self.f_alt);
        w.u16(self.ix);
        w.u16(self.iy);
        w.u16(self.sp);
        w.u16(self.pc);
        w.u8(self.i);
        w.u8(self.rr);
        w.bool(self.iff1);
        w.bool(self.iff2);
        w.u8(self.im);
        w.bool(self.halted);
        w.bool(self.ei_pending);
        w.u16(self.memptr);
        w.u8(self.q);
    }

    fn load(&mut self, r: &mut ReadCursor) -> Result<(), LoadError> {
        r.bytes(&mut self.r)?;
        self.f = r.u8()?;
        r.bytes(&mut self.r_alt)?;
        self.f_alt = r.u8()?;
        self.ix = r.u16()?;
        self.iy = r.u16()?;
        self.sp = r.u16()?;
        self.pc = r.u16()?;
        self.i = r.u8()?;
        self.rr = r.u8()?;
        self.iff1 = r.bool()?;
        self.iff2 = r.bool()?;
        self.im = r.u8()?;
        if self.im > 2 {
            return Err(LoadError::BadValue("z80 interrupt mode"));
        }
        self.halted = r.bool()?;
        self.ei_pending = r.bool()?;
        self.memptr = r.u16()?;
        self.q = r.u8()?;
        Ok(())
    }
}
