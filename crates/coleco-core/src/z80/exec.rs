// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! Z80 instruction decode and execution.
//!
//! The decode follows the opcode's octal structure, which is what makes the
//! chip's table regular: `xx yyy zzz`, with `p = yyy >> 1` and `q = yyy & 1`.
//! Every group below is that split, not a 256-way jump table, because the
//! structure is the documentation.
//!
//! A child module of [`super`] so it can reach the private register file while
//! keeping the state definition and the several hundred lines of decode in
//! separate files.

use super::{flag, parity, sz53, sz53p, Bus, Index, A, B, C, L, MEM, Z80};

impl Z80 {
    /// Execute one instruction, or take an interrupt. Returns T-states.
    pub fn step<Bs: Bus>(&mut self, bus: &mut Bs) -> i32 {
        self.cycles = 0;

        // An interrupt is recognised between instructions, and never in the one
        // immediately after `EI`: without that delay a handler ending in
        // `EI; RET` would be re-entered before it could return.
        let can_interrupt = !self.ei_pending;
        self.ei_pending = false;
        if can_interrupt && self.iff1 && bus.irq() {
            self.take_irq(bus);
            return self.cycles;
        }

        if self.halted {
            // HALT keeps fetching, and keeps refreshing, until something wakes it.
            self.bump_refresh();
            self.tick(4);
            return self.cycles;
        }

        // Cleared here and re-latched by every helper that writes flags, so an
        // instruction that sets none leaves it zero. `SCF` and `CCF` read it.
        self.q = 0;
        let op = self.fetch(bus);
        self.exec(bus, op, Index::Hl);
        self.cycles
    }

    /// Take a non-maskable interrupt, between instructions. Returns T-states.
    ///
    /// The NMI input is edge-triggered, so remembering that an edge arrived is
    /// the caller's job: this only performs the response. It ignores IFF1 and
    /// the `EI` delay, which gate the maskable input only. IFF1 is cleared so
    /// the handler is not interrupted, and IFF2 keeps the old value, which is
    /// how `RETN` puts it back. The Mega Drive never raises it; on the Master
    /// System it is the PAUSE button.
    pub fn nmi<Bs: Bus>(&mut self, bus: &mut Bs) -> i32 {
        self.cycles = 0;
        self.halted = false;
        self.iff1 = false;
        self.ei_pending = false;
        self.bump_refresh();
        // The acknowledge is an opcode fetch that is thrown away (5 T-states),
        // then the two stack writes: 11 in all.
        self.tick(5);
        self.push16(bus, self.pc);
        self.pc = 0x0066;
        self.memptr = 0x0066;
        self.cycles
    }

    fn take_irq<Bs: Bus>(&mut self, bus: &mut Bs) {
        self.irqs_taken = self.irqs_taken.saturating_add(1);
        self.halted = false;
        self.iff1 = false;
        self.iff2 = false;
        self.bump_refresh();
        match self.im {
            // Mode 0 executes whatever the device puts on the bus. With nothing
            // driving it that is $FF, which is `RST 38h`.
            0 => {
                let v = bus.irq_vector();
                self.tick(6);
                if v == 0xff {
                    self.push16(bus, self.pc);
                    self.pc = 0x0038;
                    self.memptr = 0x0038;
                } else {
                    self.exec(bus, v, Index::Hl);
                }
            }
            1 => {
                self.tick(7);
                self.push16(bus, self.pc);
                self.pc = 0x0038;
                self.memptr = 0x0038;
            }
            _ => {
                // Mode 2: I supplies the high byte of a vector-table address and
                // the device the low byte.
                let v = bus.irq_vector();
                self.tick(7);
                self.push16(bus, self.pc);
                let addr = u16::from_be_bytes([self.i, v & 0xfe]);
                let lo = self.read_mem(bus, addr);
                let hi = self.read_mem(bus, addr.wrapping_add(1));
                self.pc = u16::from_le_bytes([lo, hi]);
                self.memptr = self.pc;
            }
        }
    }

    fn exec<Bs: Bus>(&mut self, bus: &mut Bs, op: u8, idx: Index) {
        let x = op >> 6;
        let y = ((op >> 3) & 7) as usize;
        let z = (op & 7) as usize;
        let p = y >> 1;
        let q = y & 1;

        match x {
            0 => self.exec_x0(bus, y, z, p, q, idx),
            1 => {
                if op == 0x76 {
                    self.halted = true;
                } else {
                    // `LD r,r'`. Only one operand can be memory, so the address
                    // is computed once and the *other* side keeps its plain
                    // register meaning under an index prefix.
                    let touches_mem = y == MEM || z == MEM;
                    let mem = if touches_mem {
                        self.mem_addr(bus, idx)
                    } else {
                        0
                    };
                    let src_idx = if touches_mem { Index::Hl } else { idx };
                    let v = self.get_r(bus, z, if z == MEM { idx } else { src_idx }, mem);
                    self.set_r(bus, y, if y == MEM { idx } else { src_idx }, mem, v);
                }
            }
            2 => {
                let mem = if z == MEM { self.mem_addr(bus, idx) } else { 0 };
                let v = self.get_r(bus, z, idx, mem);
                self.alu(y, v);
            }
            _ => self.exec_x3(bus, y, z, p, q, idx),
        }
    }

    fn alu(&mut self, kind: usize, v: u8) {
        match kind {
            0 => self.add8(v, false),
            1 => self.add8(v, self.f & flag::C != 0),
            2 => self.sub8(v, false, true),
            3 => self.sub8(v, self.f & flag::C != 0, true),
            4 => self.and8(v),
            5 => self.xor8(v),
            6 => self.or8(v),
            _ => self.sub8(v, false, false), // CP
        }
        self.q = self.f;
    }

    fn exec_x0<Bs: Bus>(
        &mut self,
        bus: &mut Bs,
        y: usize,
        z: usize,
        p: usize,
        q: usize,
        idx: Index,
    ) {
        match z {
            0 => match y {
                0 => {} // NOP
                1 => {
                    // EX AF,AF'
                    self.swap_af();
                }
                2 => {
                    self.tick(1);
                    let d = self.read_operand(bus) as i8 as i16;
                    self.dec_b();
                    if self.reg(B) != 0 {
                        self.tick(5);
                        self.jump_relative(d);
                    }
                }
                3 => {
                    let d = self.read_operand(bus) as i8 as i16;
                    self.tick(5);
                    self.jump_relative(d);
                }
                _ => {
                    let d = self.read_operand(bus) as i8 as i16;
                    if self.condition((y - 4) as u8) {
                        self.tick(5);
                        self.jump_relative(d);
                    }
                }
            },
            1 => {
                if q == 0 {
                    let v = self.fetch16(bus);
                    self.set_rp(p, idx, v);
                } else {
                    let a = self.idx16(idx);
                    let b = self.get_rp(p, idx);
                    let v = self.add16(a, b);
                    self.set_idx16(idx, v);
                }
            }
            2 => self.exec_x0_z2(bus, p, q, idx),
            3 => {
                let v = self.get_rp(p, idx);
                let nv = if q == 0 {
                    v.wrapping_add(1)
                } else {
                    v.wrapping_sub(1)
                };
                self.set_rp(p, idx, nv);
                self.tick(2);
            }
            4 | 5 => {
                let mem = if y == MEM { self.mem_addr(bus, idx) } else { 0 };
                let v = self.get_r(bus, y, idx, mem);
                if y == MEM {
                    self.tick(1);
                }
                let nv = if z == 4 { self.inc8(v) } else { self.dec8(v) };
                self.set_r(bus, y, idx, mem, nv);
            }
            6 => {
                // `LD r,n`. Under an index prefix the displacement comes
                // *before* the immediate, which is the one place operand order
                // in the instruction stream is not the order it reads.
                let mem = if y == MEM {
                    self.mem_addr_no_tick(bus, idx)
                } else {
                    0
                };
                let n = self.read_operand(bus);
                if y == MEM && idx != Index::Hl {
                    self.tick(2);
                }
                self.set_r(bus, y, idx, mem, n);
            }
            _ => match y {
                0 => self.rlca(),
                1 => self.rrca(),
                2 => self.rla(),
                3 => self.rra(),
                4 => self.daa(),
                5 => self.cpl(),
                6 => self.scf_ccf(false),
                _ => self.scf_ccf(true),
            },
        }
    }

    /// The `x=0, z=2` block: the indirect loads and stores of A and HL.
    fn exec_x0_z2<Bs: Bus>(&mut self, bus: &mut Bs, p: usize, q: usize, idx: Index) {
        match (q, p) {
            (0, 0) | (0, 1) => {
                let a = if p == 0 { self.bc() } else { self.de() };
                let av = self.reg(A);
                self.write_mem(bus, a, av);
                // The latch takes A in its high byte and the incremented low
                // byte of the address below it, which is only observable through
                // a later `BIT n,(HL)`.
                self.memptr = ((av as u16) << 8) | (a.wrapping_add(1) & 0xff);
            }
            (0, 2) => {
                let a = self.fetch16(bus);
                let v = self.idx16(idx);
                self.write_mem(bus, a, v as u8);
                self.write_mem(bus, a.wrapping_add(1), (v >> 8) as u8);
                self.memptr = a.wrapping_add(1);
            }
            (0, _) => {
                let a = self.fetch16(bus);
                let av = self.reg(A);
                self.write_mem(bus, a, av);
                self.memptr = ((av as u16) << 8) | (a.wrapping_add(1) & 0xff);
            }
            (_, 0) | (_, 1) => {
                let a = if p == 0 { self.bc() } else { self.de() };
                let v = self.read_mem(bus, a);
                self.set_reg(A, v);
                self.memptr = a.wrapping_add(1);
            }
            (_, 2) => {
                let a = self.fetch16(bus);
                let lo = self.read_mem(bus, a);
                let hi = self.read_mem(bus, a.wrapping_add(1));
                self.set_idx16(idx, u16::from_le_bytes([lo, hi]));
                self.memptr = a.wrapping_add(1);
            }
            _ => {
                let a = self.fetch16(bus);
                let v = self.read_mem(bus, a);
                self.set_reg(A, v);
                self.memptr = a.wrapping_add(1);
            }
        }
    }

    fn exec_x3<Bs: Bus>(
        &mut self,
        bus: &mut Bs,
        y: usize,
        z: usize,
        p: usize,
        q: usize,
        idx: Index,
    ) {
        match z {
            0 => {
                self.tick(1);
                if self.condition(y as u8) {
                    self.pc = self.pop16(bus);
                    self.memptr = self.pc;
                }
            }
            1 => {
                if q == 0 {
                    let v = self.pop16(bus);
                    self.set_rp2(p, idx, v);
                } else {
                    match p {
                        0 => {
                            self.pc = self.pop16(bus);
                            self.memptr = self.pc;
                        }
                        1 => self.swap_exx(),
                        2 => self.pc = self.idx16(idx),
                        _ => {
                            self.sp = self.idx16(idx);
                            self.tick(2);
                        }
                    }
                }
            }
            2 => {
                let a = self.fetch16(bus);
                self.memptr = a;
                if self.condition(y as u8) {
                    self.pc = a;
                }
            }
            3 => self.exec_x3_z3(bus, y, idx),
            4 => {
                let a = self.fetch16(bus);
                self.memptr = a;
                if self.condition(y as u8) {
                    self.tick(1);
                    self.push16(bus, self.pc);
                    self.pc = a;
                }
            }
            5 => {
                if q == 0 {
                    self.tick(1);
                    let v = self.get_rp2(p, idx);
                    self.push16(bus, v);
                } else {
                    match p {
                        0 => {
                            let a = self.fetch16(bus);
                            self.memptr = a;
                            self.tick(1);
                            self.push16(bus, self.pc);
                            self.pc = a;
                        }
                        1 => {
                            let op2 = self.fetch(bus);
                            self.exec_dd_fd(bus, op2, Index::Ix);
                        }
                        2 => {
                            let op2 = self.fetch(bus);
                            self.exec_ed(bus, op2);
                        }
                        _ => {
                            let op2 = self.fetch(bus);
                            self.exec_dd_fd(bus, op2, Index::Iy);
                        }
                    }
                }
            }
            6 => {
                let n = self.read_operand(bus);
                self.alu(y, n);
            }
            _ => {
                self.tick(1);
                self.push16(bus, self.pc);
                self.pc = (y as u16) * 8;
                self.memptr = self.pc;
            }
        }
    }

    /// The `x=3, z=3` block: the odds and ends, including the CB prefix.
    fn exec_x3_z3<Bs: Bus>(&mut self, bus: &mut Bs, y: usize, idx: Index) {
        match y {
            0 => {
                let a = self.fetch16(bus);
                self.memptr = a;
                self.pc = a;
            }
            1 => {
                let op2 = self.fetch(bus);
                self.exec_cb(bus, op2, Index::Hl, 0);
            }
            2 => {
                // `OUT (n),A`. The port's high byte is A, and the latch takes A
                // above the incremented port number.
                let n = self.read_operand(bus);
                let av = self.reg(A);
                let port = u16::from_be_bytes([av, n]);
                self.tick(4);
                bus.output(port, av);
                self.memptr = ((av as u16) << 8) | (n.wrapping_add(1) as u16);
            }
            3 => {
                let n = self.read_operand(bus);
                let av = self.reg(A);
                let port = u16::from_be_bytes([av, n]);
                self.tick(4);
                let v = bus.input(port);
                self.set_reg(A, v);
                self.memptr = port.wrapping_add(1);
            }
            4 => {
                // EX (SP),HL
                let lo = self.read_mem(bus, self.sp);
                let hi = self.read_mem(bus, self.sp.wrapping_add(1));
                let v = self.idx16(idx);
                self.tick(1);
                self.write_mem(bus, self.sp.wrapping_add(1), (v >> 8) as u8);
                self.write_mem(bus, self.sp, v as u8);
                self.tick(2);
                let nv = u16::from_le_bytes([lo, hi]);
                self.set_idx16(idx, nv);
                self.memptr = nv;
            }
            5 => self.swap_de_hl(), // EX DE,HL is never an index register
            6 => {
                self.iff1 = false;
                self.iff2 = false;
            }
            _ => {
                self.iff1 = true;
                self.iff2 = true;
                self.ei_pending = true;
            }
        }
    }

    /// A DD or FD prefix. It only means anything for instructions that name HL,
    /// `(HL)`, H or L; for everything else the prefix is ignored and the opcode
    /// runs as normal, having cost four cycles.
    fn exec_dd_fd<Bs: Bus>(&mut self, bus: &mut Bs, op: u8, idx: Index) {
        match op {
            0xcb => {
                // DDCB: the displacement comes *before* the opcode byte.
                let d = self.read_operand(bus) as i8 as i16;
                let addr = self.idx16(idx).wrapping_add(d as u16);
                self.memptr = addr;
                let op2 = self.read_operand(bus);
                self.tick(2);
                self.exec_cb(bus, op2, idx, addr);
            }
            // A second prefix restarts the decision and discards this one.
            0xdd => {
                let op2 = self.fetch(bus);
                self.exec_dd_fd(bus, op2, Index::Ix);
            }
            0xfd => {
                let op2 = self.fetch(bus);
                self.exec_dd_fd(bus, op2, Index::Iy);
            }
            0xed => {
                let op2 = self.fetch(bus);
                self.exec_ed(bus, op2);
            }
            _ => self.exec(bus, op, idx),
        }
    }

    /// CB-prefixed: rotates, shifts and bit operations.
    ///
    /// Under an index prefix every one of these becomes a read-modify-write on
    /// `(IX+d)`, and the result is *also* copied into the register the `z` field
    /// names unless that field is `(HL)`. That undocumented copy is what
    /// ZEXALL's `LD r,RLC (IX+d)` cases check.
    fn exec_cb<Bs: Bus>(&mut self, bus: &mut Bs, op: u8, idx: Index, addr: u16) {
        let x = op >> 6;
        let y = ((op >> 3) & 7) as usize;
        let z = (op & 7) as usize;
        let indexed = idx != Index::Hl;

        let src = if indexed {
            self.read_mem(bus, addr)
        } else if z == MEM {
            let a = self.hl();
            self.read_mem(bus, a)
        } else {
            self.reg(z)
        };

        match x {
            0 => {
                let res = match y {
                    0 => self.rlc(src),
                    1 => self.rrc(src),
                    2 => self.rl(src),
                    3 => self.rr(src),
                    4 => self.sla(src),
                    5 => self.sra(src),
                    6 => self.sll(src),
                    _ => self.srl(src),
                };
                self.store_cb(bus, z, idx, addr, res);
            }
            1 => {
                // BIT. Its undocumented bits come from the operand, except for
                // the memory forms, where they come from the address latch: the
                // one instruction that makes WZ observable.
                let bit = src & (1 << y);
                let xy = if indexed || z == MEM {
                    ((self.memptr >> 8) as u8) & flag::XY
                } else {
                    src & flag::XY
                };
                self.set_flags(
                    (self.f & flag::C)
                        | flag::H
                        | if bit == 0 { flag::Z | flag::PV } else { 0 }
                        | (bit & flag::S)
                        | xy,
                );
                if indexed || z == MEM {
                    self.tick(1);
                }
            }
            2 => {
                let res = src & !(1 << y);
                self.store_cb(bus, z, idx, addr, res);
            }
            _ => {
                let res = src | (1 << y);
                self.store_cb(bus, z, idx, addr, res);
            }
        }
    }

    fn store_cb<Bs: Bus>(&mut self, bus: &mut Bs, z: usize, idx: Index, addr: u16, res: u8) {
        if idx != Index::Hl {
            self.tick(1);
            self.write_mem(bus, addr, res);
            if z != MEM {
                self.set_reg(z, res); // the undocumented register copy
            }
        } else if z == MEM {
            let a = self.hl();
            self.tick(1);
            self.write_mem(bus, a, res);
        } else {
            self.set_reg(z, res);
        }
    }

    /// ED-prefixed: 16-bit arithmetic, the block instructions, the
    /// interrupt-mode set, and the I/O with the port in BC.
    fn exec_ed<Bs: Bus>(&mut self, bus: &mut Bs, op: u8) {
        let x = op >> 6;
        let y = ((op >> 3) & 7) as usize;
        let z = (op & 7) as usize;
        let p = y >> 1;
        let q = y & 1;

        // Everything outside these blocks is undefined, and the chip treats it
        // as a two-byte NOP.
        if x == 2 {
            if y >= 4 && z <= 3 {
                self.block_op(bus, y, z);
            }
            return;
        }
        if x != 1 {
            return;
        }

        match z {
            0 => {
                // `IN r,(C)`. The `110` slot has no destination and only sets
                // flags: the undocumented `IN F,(C)`.
                let port = self.bc();
                self.tick(4);
                let v = bus.input(port);
                self.memptr = port.wrapping_add(1);
                self.set_flags((self.f & flag::C) | sz53p(v));
                if y != MEM {
                    self.set_reg(y, v);
                }
            }
            1 => {
                // `OUT (C),r`. The `110` slot outputs zero on the NMOS part the
                // Mega Drive uses.
                let port = self.bc();
                let v = if y == MEM { 0 } else { self.reg(y) };
                self.tick(4);
                bus.output(port, v);
                self.memptr = port.wrapping_add(1);
            }
            2 => {
                let a = self.hl();
                let b = self.get_rp(p, Index::Hl);
                let v = if q == 0 {
                    self.sbc16(a, b)
                } else {
                    self.adc16(a, b)
                };
                self.set_hl(v);
            }
            3 => {
                let a = self.fetch16(bus);
                self.memptr = a.wrapping_add(1);
                if q == 0 {
                    let v = self.get_rp(p, Index::Hl);
                    self.write_mem(bus, a, v as u8);
                    self.write_mem(bus, a.wrapping_add(1), (v >> 8) as u8);
                } else {
                    let lo = self.read_mem(bus, a);
                    let hi = self.read_mem(bus, a.wrapping_add(1));
                    self.set_rp(p, Index::Hl, u16::from_le_bytes([lo, hi]));
                }
            }
            4 => {
                // NEG, at all eight slots.
                let v = self.reg(A);
                self.set_reg(A, 0);
                self.sub8(v, false, true);
                self.q = self.f;
            }
            5 => {
                // RETN and RETI both restore IFF1 from IFF2.
                self.pc = self.pop16(bus);
                self.memptr = self.pc;
                self.iff1 = self.iff2;
            }
            6 => {
                self.im = match y & 3 {
                    0 | 1 => 0,
                    2 => 1,
                    _ => 2,
                };
            }
            _ => match y {
                0 => {
                    self.tick(1);
                    self.i = self.reg(A);
                }
                1 => {
                    self.tick(1);
                    self.rr = self.reg(A);
                }
                2 | 3 => {
                    self.tick(1);
                    let v = if y == 2 { self.i } else { self.rr };
                    self.set_reg(A, v);
                    // PV reports IFF2, which is how a handler reads the
                    // interrupt state it interrupted.
                    self.set_flags(
                        (self.f & flag::C) | sz53(v) | if self.iff2 { flag::PV } else { 0 },
                    );
                }
                4 | 5 => {
                    // RRD and RLD rotate a digit through (HL) and A's low nibble.
                    let a = self.hl();
                    let v = self.read_mem(bus, a);
                    self.tick(4);
                    let av = self.reg(A);
                    let (stored, new_a) = if y == 4 {
                        ((v >> 4) | (av << 4), (av & 0xf0) | (v & 0x0f))
                    } else {
                        ((v << 4) | (av & 0x0f), (av & 0xf0) | (v >> 4))
                    };
                    self.write_mem(bus, a, stored);
                    self.set_reg(A, new_a);
                    self.set_flags((self.f & flag::C) | sz53p(new_a));
                    self.memptr = a.wrapping_add(1);
                }
                _ => {} // the two NOP slots
            },
        }
    }

    /// The block instructions: LDI/LDD/CPI/CPD/INI/IND/OUTI/OUTD and their
    /// repeating forms. `y` picks increment against decrement and single against
    /// repeat; `z` picks the family.
    fn block_op<Bs: Bus>(&mut self, bus: &mut Bs, y: usize, z: usize) {
        let dec = y & 1 != 0;
        let repeat = y >= 6;
        let delta: u16 = if dec { 0xffff } else { 1 };

        match z {
            0 => {
                // LDI / LDD / LDIR / LDDR
                let hl = self.hl();
                let de = self.de();
                let v = self.read_mem(bus, hl);
                self.write_mem(bus, de, v);
                self.tick(2);
                self.set_hl(hl.wrapping_add(delta));
                self.set_de(de.wrapping_add(delta));
                let bc = self.bc().wrapping_sub(1);
                self.set_bc(bc);
                // The undocumented bits come from A plus the byte moved: bit 3
                // of that sum into X, and bit 1 into Y.
                let n = self.reg(A).wrapping_add(v);
                self.set_flags(
                    (self.f & (flag::S | flag::Z | flag::C))
                        | (n & flag::X)
                        | if n & 0x02 != 0 { flag::Y } else { 0 }
                        | if bc != 0 { flag::PV } else { 0 },
                );
                if repeat && bc != 0 {
                    self.tick(5);
                    self.pc = self.pc.wrapping_sub(2);
                    self.memptr = self.pc.wrapping_add(1);
                }
            }
            1 => {
                // CPI / CPD / CPIR / CPDR
                let hl = self.hl();
                let v = self.read_mem(bus, hl);
                self.tick(5);
                self.set_hl(hl.wrapping_add(delta));
                let bc = self.bc().wrapping_sub(1);
                self.set_bc(bc);
                let av = self.reg(A);
                let res = av.wrapping_sub(v);
                let half = (av & 0x0f) < (v & 0x0f);
                let n = res.wrapping_sub(u8::from(half));
                self.set_flags(
                    (self.f & flag::C)
                        | flag::N
                        | (res & flag::S)
                        | if res == 0 { flag::Z } else { 0 }
                        | if half { flag::H } else { 0 }
                        | (n & flag::X)
                        | if n & 0x02 != 0 { flag::Y } else { 0 }
                        | if bc != 0 { flag::PV } else { 0 },
                );
                self.memptr = self.memptr.wrapping_add(delta);
                if repeat && bc != 0 && res != 0 {
                    self.tick(5);
                    self.pc = self.pc.wrapping_sub(2);
                    self.memptr = self.pc.wrapping_add(1);
                }
            }
            2 => {
                // INI / IND / INIR / INDR
                self.tick(1);
                let port = self.bc();
                self.tick(4);
                let v = bus.input(port);
                let hl = self.hl();
                self.write_mem(bus, hl, v);
                self.memptr = port.wrapping_add(delta);
                self.dec_b();
                self.set_hl(hl.wrapping_add(delta));
                let k = self.reg(C).wrapping_add(delta as u8);
                self.io_block_flags(v, k);
                if repeat && self.reg(B) != 0 {
                    self.tick(5);
                    self.pc = self.pc.wrapping_sub(2);
                }
            }
            _ => {
                // OUTI / OUTD / OTIR / OTDR
                self.tick(1);
                let hl = self.hl();
                let v = self.read_mem(bus, hl);
                self.dec_b();
                let port = self.bc();
                self.tick(4);
                bus.output(port, v);
                self.set_hl(hl.wrapping_add(delta));
                self.memptr = port.wrapping_add(delta);
                let k = self.reg(L);
                self.io_block_flags(v, k);
                if repeat && self.reg(B) != 0 {
                    self.tick(5);
                    self.pc = self.pc.wrapping_sub(2);
                }
            }
        }
    }

    /// Flags for the I/O block instructions. H and C both come from a nine-bit
    /// sum of the byte transferred and a second value that differs per
    /// instruction, and PV is a parity fold of that sum with B.
    fn io_block_flags(&mut self, v: u8, k: u8) {
        let sum = v as u16 + k as u16;
        let b = self.reg(B);
        self.set_flags(
            sz53(b)
                | if v & 0x80 != 0 { flag::N } else { 0 }
                | if sum > 0xff { flag::H | flag::C } else { 0 }
                | if parity(((sum & 7) as u8) ^ b) {
                    flag::PV
                } else {
                    0
                },
        );
    }

    // ---- register-pair helpers ----

    /// The `rp` table: BC, DE, HL/IX/IY, SP.
    fn get_rp(&self, p: usize, idx: Index) -> u16 {
        match p {
            0 => self.bc(),
            1 => self.de(),
            2 => self.idx16(idx),
            _ => self.sp,
        }
    }

    fn set_rp(&mut self, p: usize, idx: Index, v: u16) {
        match p {
            0 => self.set_bc(v),
            1 => self.set_de(v),
            2 => self.set_idx16(idx, v),
            _ => self.sp = v,
        }
    }

    /// The `rp2` table, used by PUSH and POP: the last entry is AF, not SP.
    fn get_rp2(&self, p: usize, idx: Index) -> u16 {
        match p {
            0 => self.bc(),
            1 => self.de(),
            2 => self.idx16(idx),
            _ => u16::from_be_bytes([self.reg(A), self.f]),
        }
    }

    fn set_rp2(&mut self, p: usize, idx: Index, v: u16) {
        match p {
            0 => self.set_bc(v),
            1 => self.set_de(v),
            2 => self.set_idx16(idx, v),
            _ => {
                let b = v.to_be_bytes();
                self.set_reg(A, b[0]);
                self.f = b[1];
            }
        }
    }

    fn jump_relative(&mut self, d: i16) {
        self.pc = self.pc.wrapping_add(d as u16);
        self.memptr = self.pc;
    }

    fn dec_b(&mut self) {
        let v = self.reg(B).wrapping_sub(1);
        self.set_reg(B, v);
    }
}
