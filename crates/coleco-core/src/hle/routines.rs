// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The BIOS routines, reimplemented.
//!
//! Each one is written from what the real routine DOES, learned by reading it
//! as the oracle, and then held to it by `routinediff`, which replays every
//! call the corpus makes on both and compares the whole machine. The code is
//! ours; the behaviour, down to quirks games were written against, is the
//! real BIOS's.
//!
//! Registers and flags are left as the real routine leaves them where that
//! is cheap to state, because a game is free to read them after the call.
//!
//! Cycles charged are each routine's measured average on the real BIOS, so a
//! game's frame budget comes out about the same. Measured by `routinediff`.

use crate::machine::ColecoBus;
use crate::z80::Z80;

/// Flags as `CP n` leaves them with `A` = `a`: the subtraction's S, Z, H, V,
/// N and C, with bits 5 and 3 taken from the OPERAND, which is where CP
/// differs from every other ALU instruction.
pub fn cp_flags(a: u8, n: u8) -> u8 {
    let res = a.wrapping_sub(n);
    let mut f = 0x02; // N
    if res & 0x80 != 0 {
        f |= 0x80;
    }
    if res == 0 {
        f |= 0x40;
    }
    if (a & 0x0f) < (n & 0x0f) {
        f |= 0x10;
    }
    if (a ^ n) & (a ^ res) & 0x80 != 0 {
        f |= 0x04;
    }
    if a < n {
        f |= 0x01;
    }
    f | (n & 0x28)
}

/// S, Z, the undocumented bits 5 and 3, and parity, of a result.
fn sz53p(v: u8) -> u8 {
    let mut f = v & 0xa8;
    if v == 0 {
        f |= 0x40;
    }
    if v.count_ones() % 2 == 0 {
        f |= 0x04;
    }
    f
}

fn b(cpu: &Z80) -> u8 {
    (cpu.bc() >> 8) as u8
}

fn set_bc(cpu: &mut Z80, b: u8, c: u8) {
    cpu.set_bc(u16::from_be_bytes([b, c]));
}

fn ram16(bus: &mut ColecoBus, addr: u16) -> u16 {
    let lo = bus.peek(addr);
    let hi = bus.peek(addr.wrapping_add(1));
    u16::from_le_bytes([lo, hi])
}

fn set_ram16(bus: &mut ColecoBus, addr: u16, v: u16) {
    use crate::z80::Bus;
    let [lo, hi] = v.to_le_bytes();
    bus.write(addr, lo);
    bus.write(addr.wrapping_add(1), hi);
}

/// Where the BIOS keeps each VRAM table's base address, by table code:
/// 0 sprite attributes, 1 sprite patterns, 2 names, 3 patterns, 4 colours.
const TABLE_BASES: u16 = 0x73f2;
/// Graphics mode 2 is in force: register 0's shadow, bit 1.
fn mode2(bus: &ColecoBus) -> bool {
    bus.ram[0x3c3] & 0x02 != 0
}

/// Run the routine at `target`, if it is written. `Some(cycles)` when it ran
/// (the caller then performs the RET), `None` when it is not written yet.
pub fn call(target: u16, cpu: &mut Z80, bus: &mut ColecoBus) -> Option<i32> {
    Some(match target {
        0x1cca => write_register(cpu, bus),
        0x1d57 => read_register(cpu, bus),
        0x1d01 => write_vram(cpu, bus),
        0x1d3e => read_vram(cpu, bus),
        0x18d4 => fill_vram(cpu, bus),
        0x1b1d => init_table(cpu, bus),
        0x1ba3 => get_vram(cpu, bus),
        0x1c27 => put_vram(cpu, bus),
        0x1927 => load_ascii(cpu, bus),
        0x003b => rand_gen(cpu, bus),
        0x18e9 => mode_1(cpu, bus),
        0x1c66 => init_spr_order(cpu, bus),
        0x1c82 => wr_spr_nm_tbl(cpu, bus),
        _ => return None,
    })
}

/// READ_REGISTER (`$1FDC`): A := VDP status. Reading it clears the frame
/// flag, and with it the interrupt, which is why games call it.
fn read_register(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    cpu.set_a(bus.vdp.read_control());
    21
}

/// How the BIOS's block transfers count. A count in DE moves E bytes first
/// (256 when E is 0), then the loop decrements D and stops if the result is
/// NEGATIVE or zero, moving 256 more otherwise. So a count over 255 that is
/// not a multiple of 256 moves 256 bytes fewer than asked, and a count with
/// D of `$81` or more moves only E bytes. Games were written against both.
/// Returns the bytes moved and D as the loop leaves it.
fn block_count(count: u16) -> (usize, u8) {
    let e = (count & 0xff) as usize;
    let mut d = (count >> 8) as u8;
    let mut n = if e == 0 { 256 } else { e };
    loop {
        d = d.wrapping_sub(1);
        if d & 0x80 != 0 || d == 0 {
            return (n, d);
        }
        n += 256;
    }
}

/// Flags as the block loops leave them: their last instruction is DEC D,
/// with carry kept from the last I/O instruction's nine-bit sum.
fn block_flags(d_final: u8, carry: bool) -> u8 {
    let before = d_final.wrapping_add(1);
    let mut f = (d_final & 0xa8) | 0x02;
    if d_final == 0 {
        f |= 0x40;
    }
    if before & 0x0f == 0 {
        f |= 0x10;
    }
    if before == 0x80 {
        f |= 0x04;
    }
    f | u8::from(carry)
}

/// WRITE_VRAM (`$1FDF`): copy BC bytes from HL in RAM to DE in VRAM, with
/// the block count's quirk.
fn write_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    let (src, dst, count) = (cpu.hl(), cpu.de(), cpu.bc());
    let ctl = dst.wrapping_add(0x4000);
    bus.vdp.write_control(ctl as u8);
    bus.vdp.write_control((ctl >> 8) as u8);
    let (n, d) = block_count(count);
    let mut last = 0;
    for i in 0..n {
        last = bus.read(src.wrapping_add(i as u16));
        bus.vdp.write_data(last);
    }
    let end = src.wrapping_add(n as u16);
    cpu.set_a((ctl >> 8) as u8);
    set_bc(cpu, 0, 0xbe);
    cpu.set_de(u16::from_be_bytes([d, count as u8]));
    cpu.set_hl(end);
    cpu.f = block_flags(d, last as u16 + (end & 0xff) > 0xff);
    60 + 34 * n as i32
}

/// READ_VRAM (`$1FE2`): copy BC bytes from DE in VRAM to HL in RAM.
fn read_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    let (dst, src, count) = (cpu.hl(), cpu.de(), cpu.bc());
    bus.vdp.write_control(src as u8);
    bus.vdp.write_control((src >> 8) as u8);
    let (n, d) = block_count(count);
    let mut last = 0;
    for i in 0..n {
        last = bus.vdp.read_data();
        bus.write(dst.wrapping_add(i as u16), last);
    }
    cpu.set_a((src >> 8) as u8);
    set_bc(cpu, 0, 0xbe);
    cpu.set_de(u16::from_be_bytes([d, count as u8]));
    cpu.set_hl(dst.wrapping_add(n as u16));
    cpu.f = block_flags(d, last as u16 + 0xbf > 0xff);
    60 + 34 * n as i32
}

/// FILL_VRAM (`$1F82`): write A to DE bytes of VRAM from HL (65536 when DE
/// is 0), then read the status register, which clears a pending frame
/// flag: a side effect games get whether they want it or not.
fn fill_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let (v, addr, count) = (cpu.a(), cpu.hl(), cpu.de());
    bus.vdp.write_control(addr as u8);
    bus.vdp.write_control((addr >> 8) as u8 | 0x40);
    let n = if count == 0 { 0x10000 } else { count as u32 };
    for _ in 0..n {
        bus.vdp.write_data(v);
    }
    set_bc(cpu, b(cpu), v);
    cpu.set_de(0);
    // The loop ends on OR E with DE at zero; the status read leaves flags.
    cpu.f = 0x44;
    // The real fill takes most of a frame for a whole table, and reads the
    // status at the END, clearing a frame flag raised on the way. So the
    // time passes first, then the read.
    bus.spend(60 + 41 * n as i32);
    cpu.set_a(bus.vdp.read_control());
    30
}

/// INIT_TABLE (`$1FB8`): VRAM table A lives at HL. Records the address at
/// `$73F2 + 2A` and points the VDP register at it. In graphics mode 2 the
/// pattern and colour tables can only sit at 0 or `$2000`, and their
/// registers carry the mask bits set.
fn init_table(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let code = cpu.a();
    let hl = cpu.hl();
    let slot = TABLE_BASES.wrapping_add(2 * code as u16);
    set_ram16(bus, slot, hl);
    cpu.ix = slot;
    let (reg, val) = match (mode2(bus), code) {
        (true, 3) => (4, if hl == 0 { 0x03 } else { 0x07 }),
        (true, 4) => (3, if hl == 0 { 0x7f } else { 0xff }),
        _ => {
            // Per table: how far to shift the address, and which register.
            const SHIFT_REG: [(u32, u8); 5] = [(7, 5), (11, 6), (10, 2), (11, 4), (6, 3)];
            let (shift, reg) = SHIFT_REG[(code as usize).min(4)];
            cpu.iy = 0x1b76u16.wrapping_add(2 * code as u16);
            let shifted = hl >> shift;
            cpu.set_hl(shifted);
            (reg, shifted as u8)
        }
    };
    set_bc(cpu, reg, val);
    200 + write_register(cpu, bus)
}

/// The address arithmetic GET_VRAM and PUT_VRAM share: item index DE and
/// item count IY of table A, scaled by the table's item size, onto the
/// table's base. Leaves DE = VRAM address, BC = byte count, HL untouched.
/// The colour table's items are single bytes outside graphics mode 2.
fn table_address(cpu: &mut Z80, bus: &mut ColecoBus) {
    let code = cpu.a();
    let mut index = cpu.de();
    let mut count = cpu.iy;
    let scaled = !(code == 4 && !mode2(bus));
    if scaled {
        const SHIFTS: [u32; 5] = [2, 3, 0, 3, 3];
        let shift = SHIFTS[(code as usize).min(4)];
        cpu.iy = 0x1bffu16.wrapping_add(code as u16);
        index <<= shift;
        count <<= shift;
        cpu.set_a(0);
    } else {
        cpu.set_a(bus.ram[0x3c3]);
    }
    set_ram16(bus, 0x73fe, count);
    let slot = TABLE_BASES.wrapping_add(2 * code as u16);
    cpu.ix = slot;
    let base = ram16(bus, slot);
    cpu.set_de(base.wrapping_add(index));
    cpu.set_bc(count);
}

/// GET_VRAM (`$1FBB`): read IY items of table A from item DE into HL.
fn get_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    table_address(cpu, bus);
    120 + read_vram(cpu, bus)
}

/// PUT_VRAM (`$1FBE`): write IY items of table A from HL at item DE. With
/// sprite multiplexing on (`$73C7` = 1), sprite attributes go to the RAM
/// copy the cartridge header points at (`$8002`) instead of VRAM.
fn put_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    if cpu.a() == 0 && bus.ram[0x3c7] == 1 {
        // The real one scales only the low bytes of index and count, by 4.
        let src = cpu.hl();
        let e = (cpu.de() as u8).wrapping_shl(2);
        let dst = ram16(bus, 0x8002).wrapping_add(u16::from_be_bytes([(cpu.de() >> 8) as u8, e]));
        let cnt_lo = (cpu.iy as u8).wrapping_shl(2);
        let count = u16::from_be_bytes([(cpu.iy >> 8) as u8, cnt_lo]);
        let n = if count == 0 { 0x10000 } else { count as u32 };
        let mut last = 0;
        for i in 0..n {
            last = bus.read(src.wrapping_add(i as u16));
            bus.write(dst.wrapping_add(i as u16), last);
        }
        cpu.set_a(cnt_lo);
        cpu.set_hl(src.wrapping_add(n as u16));
        cpu.set_de(dst.wrapping_add(n as u16));
        cpu.set_bc(0);
        // LDIR: S, Z and C as SLA left them, H and N clear, PV clear at the
        // end, bits 3 and 1 of A plus the last byte into X and Y.
        let sla = sz53p(cnt_lo) & !0x04 | (cnt_lo >> 7) as u8;
        let k = cnt_lo.wrapping_add(last);
        cpu.f = (sla & 0xc1) | (k & 0x08) | if k & 0x02 != 0 { 0x20 } else { 0 };
        return 80 + 21 * n as i32;
    }
    table_address(cpu, bus);
    120 + write_vram(cpu, bus)
}

/// LOAD_ASCII (`$1F7F`): the font into the pattern table, characters `$1D`
/// onward, then the space glyph into pattern 0. The glyphs are whatever sits
/// at `$158B` in the image: ours.
fn load_ascii(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    cpu.set_hl(0x158b);
    cpu.set_de(0x1d);
    cpu.iy = 0x60;
    cpu.set_a(3);
    let c1 = put_vram(cpu, bus);
    cpu.set_hl(0x15a3);
    cpu.set_de(0);
    cpu.iy = 1;
    cpu.set_a(3);
    40 + c1 + put_vram(cpu, bus)
}

/// MODE_1 (`$1F85`): graphics mode 2 with the standard table layout: names
/// at `$1800`, colours `$2000`, patterns 0, sprite attributes `$1B00`,
/// sprite patterns `$3800`, display off, black backdrop. Seven calls to
/// routines above, as the real one makes them.
fn mode_1(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let mut c = 0;
    set_bc(cpu, 0, 0x00);
    c += write_register(cpu, bus);
    set_bc(cpu, 1, 0x80);
    c += write_register(cpu, bus);
    for (code, addr) in [(2u8, 0x1800u16), (4, 0x2000), (3, 0x0000), (0, 0x1b00), (1, 0x3800)] {
        cpu.set_a(code);
        cpu.set_hl(addr);
        c += init_table(cpu, bus);
    }
    set_bc(cpu, 7, 0x00);
    c + write_register(cpu, bus) + 100
}

/// INIT_SPR_ORDER (`$1FC1`): the sprite order table the header points at
/// (`$8004`) := 0, 1, ... A-1 (256 entries when A is 0).
fn init_spr_order(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    let count = cpu.a();
    let buf = ram16(bus, 0x8004);
    let n = if count == 0 { 256 } else { count as usize };
    for i in 0..n {
        bus.write(buf.wrapping_add(i as u16), i as u8);
    }
    set_bc(cpu, count, cpu.bc() as u8);
    cpu.set_hl(buf.wrapping_add(n as u16));
    cpu.set_a(count);
    cpu.f = cp_flags(count, count);
    40 + 30 * n as i32
}

/// WR_SPR_NM_TBL (`$1FC4`): write A sprites to the VRAM attribute table, in
/// the order the order table (`$8004`) gives, from the RAM attribute copy
/// (`$8002`), four bytes each.
fn wr_spr_nm_tbl(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    let count = cpu.a();
    let order = ram16(bus, 0x8004);
    let attrs = ram16(bus, 0x8002);
    let base = ram16(bus, TABLE_BASES);
    bus.vdp.write_control(base as u8);
    bus.vdp.write_control((base >> 8) as u8 | 0x40);
    let n = if count == 0 { 256 } else { count as usize };
    let (mut hl, mut last) = (attrs, 0u8);
    for i in 0..n {
        let idx = bus.read(order.wrapping_add(i as u16));
        hl = attrs.wrapping_add(4 * idx as u16);
        for _ in 0..4 {
            last = bus.read(hl);
            bus.vdp.write_data(last);
            hl = hl.wrapping_add(1);
        }
    }
    cpu.ix = order.wrapping_add(n as u16);
    cpu.iy = TABLE_BASES;
    cpu.set_de(base);
    cpu.set_hl(hl);
    set_bc(cpu, 0, 0xbe);
    cpu.set_a(0);
    // DEC A from 1 to 0 ends the loop; carry from the last OUTI.
    cpu.f = 0x42 | u8::from(last as u16 + (hl & 0xff) > 0xff);
    80 + 150 * n as i32
}

/// RAND_GEN (`$1FFD`): a 16-bit shift register at `$73C8`, fed back from
/// bits 15 and 8, shifted left. A := the new low byte.
fn rand_gen(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let hl = ram16(bus, 0x73c8);
    let feedback = ((hl >> 15) ^ (hl >> 8)) & 1;
    let next = (hl << 1) | feedback;
    set_ram16(bus, 0x73c8, next);
    cpu.set_hl(next);
    cpu.set_a(next as u8);
    // The last instruction to set flags is RL H.
    cpu.f = sz53p((next >> 8) as u8) | (hl >> 15) as u8;
    90
}

/// WRITE_REGISTER (`$1FD9`): VDP register B := C. Registers 0 and 1 are also
/// kept in RAM at `$73C3`/`$73C4`, because the chip's registers cannot be
/// read back and other routines need them.
fn write_register(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let (b, c) = ((cpu.bc() >> 8) as u8, cpu.bc() as u8);
    bus.vdp.write_control(c);
    bus.vdp.write_control(b.wrapping_add(0x80));
    match b {
        0 => bus.ram[0x3c3] = c,
        1 => bus.ram[0x3c4] = c,
        _ => {}
    }
    // Left as the real one leaves them: the last test is B against 1, and
    // register 1's path reloads A with the value.
    cpu.set_a(if b == 1 { c } else { b });
    cpu.f = cp_flags(b, 1);
    100
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block count as the real loop computes it, including both quirks.
    #[test]
    fn block_counts_follow_the_real_loop() {
        assert_eq!(block_count(0x0005), (5, 0xff));
        assert_eq!(block_count(0x0000), (256, 0xff), "E of 0 is 256");
        assert_eq!(block_count(0x0100), (256, 0x00));
        assert_eq!(block_count(0x0105), (5, 0x00), "256 fewer than asked");
        assert_eq!(block_count(0x0205), (261, 0x00));
        assert_eq!(block_count(0x8060), (0x60 + 256 * 0x7f, 0x00));
        assert_eq!(block_count(0xfe60), (0x60, 0xfd), "D past $80 goes negative at once");
    }

    /// The flags DEC D leaves, checked against the Z80 core's own DEC D.
    #[test]
    fn block_flags_match_the_z80s_dec_d() {
        use crate::z80::Bus;
        struct Ram([u8; 0x10000]);
        impl Bus for Ram {
            fn read(&mut self, a: u16) -> u8 {
                self.0[a as usize]
            }
            fn write(&mut self, a: u16, v: u8) {
                self.0[a as usize] = v;
            }
            fn input(&mut self, _: u16) -> u8 {
                0xff
            }
            fn output(&mut self, _: u16, _: u8) {}
        }
        for before in 0..=255u8 {
            for carry in [false, true] {
                let mut bus = Ram([0; 0x10000]);
                bus.0[0] = 0x15; // DEC D
                let mut cpu = Z80::new();
                cpu.reset();
                cpu.set_de(u16::from(before) << 8);
                cpu.f = u8::from(carry);
                cpu.step(&mut bus);
                assert_eq!(block_flags(before.wrapping_sub(1), carry), cpu.f, "DEC D from {before:02X}");
            }
        }
    }

    #[test]
    fn cp_flags_match_the_z80s_own_cp() {
        use crate::z80::Bus;
        struct Ram([u8; 0x10000]);
        impl Bus for Ram {
            fn read(&mut self, a: u16) -> u8 {
                self.0[a as usize]
            }
            fn write(&mut self, a: u16, v: u8) {
                self.0[a as usize] = v;
            }
            fn input(&mut self, _: u16) -> u8 {
                0xff
            }
            fn output(&mut self, _: u16, _: u8) {}
        }
        // Every A against a spread of operands, CP n executed by the real Z80
        // core against the helper.
        for a in 0..=255u8 {
            for n in [0u8, 1, 0x0f, 0x10, 0x28, 0x7f, 0x80, 0x81, 0xd7, 0xff, a] {
                let mut bus = Ram([0; 0x10000]);
                bus.0[0] = 0xfe; // CP n
                bus.0[1] = n;
                let mut cpu = Z80::new();
                cpu.reset();
                cpu.set_a(a);
                cpu.step(&mut bus);
                assert_eq!(cp_flags(a, n), cpu.f, "CP {n:02X} with A={a:02X}");
            }
        }
    }
}
