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

/// What a routine did: finished, so the trap returns to its caller; or
/// handed control to game code, the PC and stack already set, as the sound
/// driver does for a game's special sound routines.
pub enum Flow {
    Ret(i32),
    Jump(i32),
}

pub(super) fn ram16(bus: &mut ColecoBus, addr: u16) -> u16 {
    let lo = bus.peek(addr);
    let hi = bus.peek(addr.wrapping_add(1));
    u16::from_le_bytes([lo, hi])
}

pub(super) fn set_ram16(bus: &mut ColecoBus, addr: u16, v: u16) {
    use crate::z80::Bus;
    let [lo, hi] = v.to_le_bytes();
    bus.write(addr, lo);
    bus.write(addr.wrapping_add(1), hi);
}

/// Where the BIOS keeps each VRAM table's base address, by table code:
/// 0 sprite attributes, 1 sprite patterns, 2 names, 3 patterns, 4 colours.
const TABLE_BASES: u16 = 0x73f2;
/// Graphics mode 2 is in force: register 0's shadow, bit 1.
pub(super) fn mode2(bus: &ColecoBus) -> bool {
    bus.ram[0x3c3] & 0x02 != 0
}

/// Run the routine at `target`, if it is written. `None` when it is not
/// written yet.
pub fn call(target: u16, cpu: &mut Z80, bus: &mut ColecoBus) -> Option<Flow> {
    use super::{objects, sound};
    match target {
        0x025e => return Some(sound::play_it(cpu, bus)),
        0x027f => return Some(sound::sound_man(cpu, bus)),
        0x06d8 => return objects::putobj(cpu, bus).map(Flow::Ret),
        0x0679 => return objects::writer(cpu, bus).map(Flow::Ret),
        _ => {}
    }
    Some(Flow::Ret(match target {
        0x0213 => sound::sound_init(cpu, bus),
        0x023b => sound::turn_off_sound(cpu, bus),
        0x0300 => sound::play_songs(cpu, bus),
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
        0x114a => controller_scan(cpu, bus),
        0x116a => update_spinner(cpu, bus),
        0x118b => decoder(cpu, bus),
        0x11c1 => poller(cpu, bus),
        0x1979 => game_opt(cpu, bus),
        0x04a3 => objects::activate(cpu, bus),
        0x0664 => objects::init_writer(cpu, bus),
        _ => return None,
    }))
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
pub(super) fn write_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
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
pub(super) fn read_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
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
pub(super) fn get_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    table_address(cpu, bus);
    120 + read_vram(cpu, bus)
}

/// PUT_VRAM (`$1FBE`): write IY items of table A from HL at item DE. With
/// sprite multiplexing on (`$73C7` = 1), sprite attributes go to the RAM
/// copy the cartridge header points at (`$8002`) instead of VRAM.
pub(super) fn put_vram(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
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

// ---- controllers ----

/// What the keypad's inverted four-bit code means: key 0-9, `$0A` for `*`,
/// `$0B` for `#`, `$0F` for none or an impossible code. Interface: this is
/// the meaning of the hardware's codes, as the real BIOS decodes them.
const KEYPAD: [u8; 16] = [0x0f, 0x06, 0x01, 0x03, 0x09, 0x00, 0x0a, 0x0f, 0x02, 0x0b, 0x07, 0x0f, 0x05, 0x04, 0x08, 0x0f];

/// Read a controller port, inverted so a pressed input reads as a 1.
fn read_pad(bus: &mut ColecoBus, player: u8) -> u8 {
    use crate::z80::Bus;
    !bus.input(if player == 0 { 0xfc } else { 0xff })
}

/// Flags as CPL leaves them: H and N set, bits 5 and 3 from A, the rest kept.
fn cpl_flags(f: u8, a: u8) -> u8 {
    (f & 0xc5) | 0x12 | (a & 0x28)
}

/// CONTROLLER_SCAN (`$1F76`): both controllers read in joystick mode into
/// `$73EE`/`$73EF`, then in keypad mode into `$73F0`/`$73F1`, inverted, and
/// the ports left in joystick mode. The four bytes 40 or more titles read.
fn controller_scan(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    let j1 = read_pad(bus, 0);
    let j2 = read_pad(bus, 1);
    bus.output(0x80, j2);
    let k1 = read_pad(bus, 0);
    let k2 = read_pad(bus, 1);
    bus.output(0xc0, k2);
    for (addr, v) in [(0x73ee, j1), (0x73ef, j2), (0x73f0, k1), (0x73f1, k2)] {
        bus.write(addr, v);
    }
    cpu.set_a(k2);
    cpu.f = cpl_flags(cpu.f, k2);
    150
}

/// UPDATE_SPINNER (`$1F88`): each controller's spinner, from port bits 4
/// (no movement when set) and 5 (direction), steps its count at `$73EB`/
/// `$73EC` down or up.
fn update_spinner(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    let mut f = cpu.f;
    for (player, addr) in [(0u8, 0x73ebu16), (1, 0x73ec)] {
        let raw = bus.input(if player == 0 { 0xfc } else { 0xff });
        cpu.set_a(raw);
        if raw & 0x10 == 0 {
            let v = bus.peek(addr);
            let next = if raw & 0x20 == 0 { v.wrapping_sub(1) } else { v.wrapping_add(1) };
            bus.write(addr, next);
            // INC/DEC (HL): S, Z, H, V, N from the result, carry kept.
            let dec = raw & 0x20 == 0;
            let half = if dec { v & 0x0f == 0 } else { v & 0x0f == 0x0f };
            let over = if dec { v == 0x80 } else { v == 0x7f };
            f = (f & 0x01)
                | (next & 0xa8)
                | if next == 0 { 0x40 } else { 0 }
                | if half { 0x10 } else { 0 }
                | if over { 0x04 } else { 0 }
                | if dec { 0x02 } else { 0 };
        } else {
            // BIT 4 set: Z clear, H set, bits 5 and 3 from the port byte.
            f = (f & 0x01) | 0x10 | (raw & 0x28);
        }
        cpu.set_hl(if player == 0 { 0x73eb } else { 0x73ec });
    }
    cpu.f = f;
    80
}

/// DECODER (`$1F79`): controller H (0 or 1), part L: 0 the joystick
/// (directions in L, fire in H, the spinner count in E, which is then
/// cleared), 1 the keypad (the decoded key in L, the other fire in H).
fn decoder(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    use crate::z80::Bus;
    let player = (cpu.hl() >> 8) as u8;
    let keypad = cpu.hl() as u8 == 1;
    let raw;
    if keypad {
        bus.output(0x80, cpu.a());
        raw = read_pad(bus, player);
        bus.output(0xc0, raw);
        let code = raw & 0x0f;
        set_bc(cpu, 0, code);
        cpu.set_hl(u16::from_be_bytes([raw & 0x40, KEYPAD[code as usize]]));
    } else {
        let spin_addr = if player == 0 { 0x73eb } else { 0x73ec };
        let spin = bus.peek(spin_addr);
        bus.write(spin_addr, 0);
        cpu.set_bc(spin_addr);
        raw = read_pad(bus, player);
        cpu.set_de(u16::from_be_bytes([raw, spin]));
        cpu.set_hl(u16::from_be_bytes([raw & 0x40, raw & 0x0f]));
    }
    if keypad {
        cpu.set_de(u16::from_be_bytes([raw, cpu.de() as u8]));
    }
    cpu.set_a(raw & 0x40);
    // AND $40 is the last flag-setter: H set, S/Z/P/5/3 from the result.
    cpu.f = sz53p(raw & 0x40) | 0x10;
    120
}

/// Flags as `AND n` leaves them: S, Z, bits 5 and 3, parity, H set.
pub(super) fn and_flags(result: u8) -> u8 {
    sz53p(result) | 0x10
}

/// Flags as `BIT n,r` leaves them for a register: Z and V set when the bit
/// is clear, S when bit 7 is tested and set, H set, bits 5 and 3 from the
/// register, carry kept.
pub(super) fn bit_flags(f: u8, n: u32, r: u8) -> u8 {
    let set = r & (1 << n) != 0;
    (f & 0x01)
        | 0x10
        | (r & 0x28)
        | if set { 0 } else { 0x44 }
        | if set && n == 7 { 0x80 } else { 0 }
}

/// Flags as `ADD rr,rr` leaves them: S, Z and V kept, H from bit 11, carry
/// from bit 15, N clear, bits 5 and 3 from the result's high byte.
fn add16_flags(f: u8, a: u16, b: u16) -> u8 {
    let r = a.wrapping_add(b);
    (f & 0xc4)
        | ((r >> 8) as u8 & 0x28)
        | if (a & 0x0fff) + (b & 0x0fff) > 0x0fff { 0x10 } else { 0 }
        | u8::from(a as u32 + b as u32 > 0xffff)
}

/// The registers POLLER works in, so it can leave them as the real one
/// does: a game may read any of them after the call.
struct Regs {
    a: u8,
    f: u8,
    b: u8,
    c: u8,
    de: u16,
    hl: u16,
    ix: u16,
    iy: u16,
}

/// One debounced input, as POLLER keeps it at `iy+slot`: the last reading
/// and whether it has been confirmed. A reading reaches the game's output
/// byte `ix+out` only when two scans in a row agree, once per change.
/// `mask` picks the input's bits from the raw byte in C; the keypad's
/// reading is decoded through the key table on the way out. Leaves A and F
/// as the real helpers do (they preserve BC, DE and HL).
fn debounce(r: &mut Regs, bus: &mut ColecoBus, mask: u8, slot: u16, out: u16, keypad: bool) {
    use crate::z80::Bus;
    let e = r.c & mask;
    let state = r.iy.wrapping_add(slot);
    let last = bus.peek(state);
    let confirmed = bus.peek(state.wrapping_add(1));
    if confirmed == 0 {
        r.f = cp_flags(e, last);
        if e != last {
            bus.write(state, e);
            r.a = e;
        } else {
            bus.write(state.wrapping_add(1), 1);
            if keypad {
                let key = KEYPAD[e as usize];
                bus.write(r.ix.wrapping_add(out), key);
                r.a = key;
                r.f = add16_flags(r.f, 0x10f5, e as u16);
            } else {
                bus.write(r.ix.wrapping_add(out), e);
                r.a = 1;
            }
        }
    } else {
        r.f = cp_flags(e, last);
        r.a = e;
        if e != last {
            bus.write(state, e);
            bus.write(state.wrapping_add(1), 0);
            r.a = 0;
            r.f = 0x44;
        }
    }
}

/// The joystick half for one player (the real one's `$1220`): directions,
/// fire, spinner, as config B asks. Raw joystick byte in A, spinner count
/// at HL.
fn poll_joystick(r: &mut Regs, bus: &mut ColecoBus) {
    use crate::z80::Bus;
    r.c = r.a;
    r.f = bit_flags(r.f, 1, r.b);
    if r.b & 0x02 != 0 {
        debounce(r, bus, 0x0f, 2, 1, false);
        r.a = r.c;
    }
    r.f = bit_flags(r.f, 0, r.b);
    if r.b & 0x01 != 0 {
        debounce(r, bus, 0x40, 0, 0, false);
        r.a = r.c;
    }
    r.f = bit_flags(r.f, 2, r.b);
    if r.b & 0x04 != 0 {
        let spin = bus.peek(r.hl);
        let acc = bus.peek(r.ix.wrapping_add(2));
        bus.write(r.ix.wrapping_add(2), spin.wrapping_add(acc));
        bus.write(r.hl, 0);
        r.a = 0;
        r.f = 0x44;
    }
}

/// The keypad half (`$123F`): the second fire and the keypad. Raw keypad-
/// mode byte in A.
fn poll_keypad(r: &mut Regs, bus: &mut ColecoBus) {
    r.c = r.a;
    r.f = bit_flags(r.f, 3, r.b);
    if r.b & 0x08 != 0 {
        debounce(r, bus, 0x40, 6, 3, false);
        r.a = r.c;
    }
    r.f = bit_flags(r.f, 4, r.b);
    if r.b & 0x10 != 0 {
        debounce(r, bus, 0x0f, 8, 4, true);
    }
}

/// POLLER (`$1FEB`): CONTROLLER_SCAN, then for each player enabled in the
/// cartridge's controller map (pointer at `$8008`: a config byte per
/// player, bit 7 enabling, then five output bytes each), the inputs the
/// config asks for, debounced: bit 0 fire, bit 1 joystick, bit 2 spinner
/// (accumulated), bit 3 the second fire, bit 4 the keypad (decoded).
/// Debounce state at `$73D7`, ten bytes per player.
fn poller(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let cycles = controller_scan(cpu, bus);
    let map = ram16(bus, 0x8008);
    let mut r = Regs {
        a: cpu.a(),
        f: cpu.f,
        b: b(cpu),
        c: cpu.bc() as u8,
        de: cpu.de(),
        hl: cpu.hl(),
        ix: map,
        iy: 0x73d7,
    };
    for player in 0..2u16 {
        if player == 1 {
            r.ix = map;
        }
        r.a = bus.peek(map.wrapping_add(player));
        r.f = bit_flags(r.f, 7, r.a);
        if r.a & 0x80 == 0 {
            if player == 0 {
                continue;
            }
            break;
        }
        r.b = r.a;
        if player == 1 {
            r.de = 10;
            r.iy = r.iy.wrapping_add(10);
        }
        let step = if player == 0 { 2 } else { 7 };
        r.de = step;
        r.f = add16_flags(r.f, r.ix, step);
        r.ix = r.ix.wrapping_add(step);
        r.a &= 0x07;
        r.f = and_flags(r.a);
        if r.a != 0 {
            r.a = bus.peek(0x73ee + player);
            r.hl = 0x73eb + player;
            poll_joystick(&mut r, bus);
        }
        r.a = r.b & 0x18;
        r.f = and_flags(r.a);
        if r.a != 0 {
            r.a = bus.peek(0x73f0 + player);
            poll_keypad(&mut r, bus);
        }
    }
    cpu.set_a(r.a);
    cpu.f = r.f;
    set_bc(cpu, r.b, r.c);
    cpu.set_de(r.de);
    cpu.set_hl(r.hl);
    cpu.ix = r.ix;
    cpu.iy = r.iy;
    cycles + 1380
}

/// Where GAME_OPT's text sits in the image (see `super::GAME_OPT_TEXT`): the
/// two headings, the option line it writes eight times, and the pieces it
/// patches in to make the other seven.
const OPT_HEADING_1: u16 = 0x1a7c;
const OPT_HEADING_2: u16 = 0x1a92;
const OPT_LINE: u16 = 0x1aa9;
const OPT_DIGITS: u16 = 0x1abf;
const OPT_TWO: u16 = 0x1ac6;
const OPT_S: u16 = 0x1ac9;

/// One PUT_VRAM into the name table: IY characters from HL at cell DE.
fn put_names(cpu: &mut Z80, bus: &mut ColecoBus, src: u16, cell: u16, count: u16) -> i32 {
    cpu.set_hl(src);
    cpu.set_de(cell);
    cpu.iy = count;
    cpu.set_a(2);
    put_vram(cpu, bus) + 40
}

/// GAME_OPT (`$1F7C`): the standard game-option screen. It only draws; the
/// game reads the keypad itself afterwards. Without it, a game that leaves
/// its menu to the BIOS shows a black screen while it waits for a key: 51
/// commercial titles call it, Tapper among them.
///
/// The real routine clears VRAM, sets MODE_1 with a dark blue backdrop,
/// loads the font, writes two headings and "1 = SKILL 1/ONE PLAYER" on eight
/// rows, then patches the option digits, the skill digits, "TWO" and a
/// plural "S" into place, colours everything white on dark blue and turns
/// the display on. The same calls, in the same order, here.
fn game_opt(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let mut c = 0;
    cpu.set_hl(0);
    cpu.set_de(0x4000);
    cpu.set_a(0);
    c += fill_vram(cpu, bus);
    c += mode_1(cpu, bus);
    set_bc(cpu, 15, 4);
    c += write_register(cpu, bus);
    c += load_ascii(cpu, bus);
    c += put_names(cpu, bus, OPT_HEADING_1, 0x25, 0x16);
    c += put_names(cpu, bus, OPT_HEADING_2, 0x65, 0x17);
    // Eight option rows: 6, 8, 10, 12 for one player, 15-21 for two.
    let rows: [u16; 8] = [0xc5, 0x105, 0x145, 0x185, 0x1e5, 0x225, 0x265, 0x2a5];
    for row in rows {
        c += put_names(cpu, bus, OPT_LINE, row, 0x16);
    }
    // The option number: 2-8 on rows two to eight.
    for (i, &row) in rows[1..].iter().enumerate() {
        c += put_names(cpu, bus, OPT_DIGITS + i as u16, row, 1);
    }
    // The skill number, column 15: 2-4 on both groups' later rows.
    for (i, cell) in [0x10f, 0x14f, 0x18f, 0x22f, 0x26f, 0x2af].into_iter().enumerate() {
        c += put_names(cpu, bus, OPT_DIGITS + (i % 3) as u16, cell, 1);
    }
    // TWO over ONE, and PLAYERS, on the two-player rows.
    for cell in [0x1f1, 0x231, 0x271, 0x2b1] {
        c += put_names(cpu, bus, OPT_TWO, cell, 3);
    }
    for cell in [0x1fb, 0x23b, 0x27b, 0x2bb] {
        c += put_names(cpu, bus, OPT_S, cell, 1);
    }
    cpu.set_hl(ram16(bus, 0x73fa));
    cpu.set_de(0x20);
    cpu.set_a(0xf4);
    c += fill_vram(cpu, bus);
    set_bc(cpu, 1, 0xc0);
    c + write_register(cpu, bus)
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

    /// Run one instruction on the real Z80 core from a given A, F, B and
    /// HL/IX, and return the flags it leaves: the oracle for the helpers.
    fn z80_flags(code: &[u8], a: u8, f: u8, b: u8, hl: u16) -> u8 {
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
        let mut bus = Ram([0; 0x10000]);
        bus.0[..code.len()].copy_from_slice(code);
        let mut cpu = Z80::new();
        cpu.reset();
        cpu.set_a(a);
        cpu.f = f;
        cpu.set_bc(u16::from(b) << 8);
        cpu.set_hl(hl);
        cpu.ix = hl;
        cpu.step(&mut bus);
        cpu.f
    }

    #[test]
    fn bit_and_add_flag_helpers_match_the_z80() {
        for v in 0..=255u8 {
            for f in [0x00u8, 0x01, 0xff] {
                for n in 0..8u32 {
                    let op = 0x40 | (n as u8) << 3; // BIT n,B
                    assert_eq!(bit_flags(f, n, v), z80_flags(&[0xcb, op], 0, f, v, 0), "BIT {n},{v:02X}");
                }
            }
            for k in [0u8, 1, 0x0f, 0x40, 0x7f, 0x80, 0xff, v] {
                assert_eq!(and_flags(v & k), z80_flags(&[0xe6, k], v, 0xff, 0, 0), "AND {k:02X}");
            }
        }
        for (x, y) in [(0x7000u16, 2u16), (0x70fe, 7), (0x0ffe, 2), (0xfffe, 7), (0x10f5, 0x0e)] {
            for f in [0x00u8, 0xff] {
                // ADD IX,DE with DE = y
                let code = [0x11, y as u8, (y >> 8) as u8, 0xdd, 0x19];
                let got = {
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
                    let mut bus = Ram([0; 0x10000]);
                    bus.0[..5].copy_from_slice(&code);
                    let mut cpu = Z80::new();
                    cpu.reset();
                    cpu.f = f;
                    cpu.ix = x;
                    cpu.step(&mut bus);
                    cpu.step(&mut bus);
                    cpu.f
                };
                assert_eq!(add16_flags(f, x, y), got, "ADD IX({x:04X}),{y:04X}");
            }
        }
    }

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
